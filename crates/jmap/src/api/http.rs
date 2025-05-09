/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{borrow::Cow, net::IpAddr, sync::Arc};

use common::{
    auth::{oauth::GrantType, AccessToken},
    core::BuildServer,
    expr::{functions::ResolveVariable, *},
    ipc::StateEvent,
    listener::{ServerInstance, SessionData, SessionManager, SessionStream},
    manager::webadmin::Resource,
    Inner, Server, KV_ACME,
};
use directory::Permission;
use http_body_util::{BodyExt, Full};
use hyper::{
    body::{self, Bytes},
    header::{self, CONTENT_TYPE},
    server::conn::http1,
    service::service_fn,
    Method, StatusCode,
};
use hyper_util::rt::TokioIo;
use jmap_proto::{
    error::request::{RequestError, RequestLimitError},
    request::{capability::Session, Request},
    response::Response,
    types::{blob::BlobId, id::Id},
};
use std::future::Future;
use store::dispatch::lookup::KeyValue;
use trc::SecurityEvent;
use utils::url_params::UrlParams;

#[cfg(feature = "enterprise")]
use crate::api::management::enterprise::telemetry::TelemetryApi;

use crate::{
    auth::{
        authenticate::{Authenticator, HttpHeaders},
        oauth::{
            auth::OAuthApiHandler, openid::OpenIdHandler, registration::ClientRegistrationHandler,
            token::TokenHandler, FormData,
        },
        rate_limit::RateLimiter,
    },
    blob::{download::BlobDownload, upload::BlobUpload, DownloadResponse, UploadResponse},
    websocket::upgrade::WebSocketUpgrade,
};

use super::{
    autoconfig::Autoconfig,
    event_source::EventSourceHandler,
    form::FormHandler,
    management::{troubleshoot::TroubleshootApi, ManagementApi, ManagementApiError},
    request::RequestHandler,
    session::SessionHandler,
    HtmlResponse, HttpRequest, HttpResponse, HttpResponseBody, JmapSessionManager, JsonResponse,
};

pub struct HttpSessionData {
    pub instance: Arc<ServerInstance>,
    pub local_ip: IpAddr,
    pub local_port: u16,
    pub remote_ip: IpAddr,
    pub remote_port: u16,
    pub is_tls: bool,
    pub session_id: u64,
}

pub trait ParseHttp: Sync + Send {
    fn parse_http_request(
        &self,
        req: HttpRequest,
        session: HttpSessionData,
    ) -> impl Future<Output = trc::Result<HttpResponse>> + Send;
}

impl ParseHttp for Server {
    async fn parse_http_request(
        &self,
        mut req: HttpRequest,
        session: HttpSessionData,
    ) -> trc::Result<HttpResponse> {
        let mut path = req.uri().path().split('/');
        path.next();

        // Validate endpoint access
        let ctx = HttpContext::new(&session, &req);
        match ctx.has_endpoint_access(self).await {
            StatusCode::OK => (),
            status => {
                // Allow loopback address to avoid lockouts
                if !session.remote_ip.is_loopback() {
                    return Ok(status.into_http_response());
                }
            }
        }

        match path.next().unwrap_or_default() {
            "jmap" => {
                match (path.next().unwrap_or_default(), req.method()) {
                    ("", &Method::POST) => {
                        // Authenticate request
                        let (_in_flight, access_token) =
                            self.authenticate_headers(&req, &session, false).await?;

                        let request = fetch_body(
                            &mut req,
                            if !access_token.has_permission(Permission::UnlimitedUploads) {
                                self.core.jmap.upload_max_size
                            } else {
                                0
                            },
                            session.session_id,
                        )
                        .await
                        .ok_or_else(|| trc::LimitEvent::SizeRequest.into_err())
                        .and_then(|bytes| {
                            Request::parse(
                                &bytes,
                                self.core.jmap.request_max_calls,
                                self.core.jmap.request_max_size,
                            )
                        })?;

                        return Ok(self
                            .handle_request(request, access_token, &session)
                            .await
                            .into_http_response());
                    }
                    ("download", &Method::GET) => {
                        // Authenticate request
                        let (_in_flight, access_token) =
                            self.authenticate_headers(&req, &session, false).await?;

                        if let (Some(_), Some(blob_id), Some(name)) = (
                            path.next().and_then(|p| Id::from_bytes(p.as_bytes())),
                            path.next().and_then(BlobId::from_base32),
                            path.next(),
                        ) {
                            return match self.blob_download(&blob_id, &access_token).await? {
                                Some(blob) => Ok(DownloadResponse {
                                    filename: name.to_string(),
                                    content_type: req
                                        .uri()
                                        .query()
                                        .and_then(|q| {
                                            form_urlencoded::parse(q.as_bytes())
                                                .find(|(k, _)| k == "accept")
                                                .map(|(_, v)| v.into_owned())
                                        })
                                        .unwrap_or("application/octet-stream".to_string()),
                                    blob,
                                }
                                .into_http_response()),
                                None => Err(trc::ResourceEvent::NotFound.into_err()),
                            };
                        }
                    }
                    ("upload", &Method::POST) => {
                        // Authenticate request
                        let (_in_flight, access_token) =
                            self.authenticate_headers(&req, &session, false).await?;

                        if let Some(account_id) =
                            path.next().and_then(|p| Id::from_bytes(p.as_bytes()))
                        {
                            return match fetch_body(
                                &mut req,
                                if !access_token.has_permission(Permission::UnlimitedUploads) {
                                    self.core.jmap.upload_max_size
                                } else {
                                    0
                                },
                                session.session_id,
                            )
                            .await
                            {
                                Some(bytes) => Ok(self
                                    .blob_upload(
                                        account_id,
                                        req.headers()
                                            .get(CONTENT_TYPE)
                                            .and_then(|h| h.to_str().ok())
                                            .unwrap_or("application/octet-stream"),
                                        &bytes,
                                        access_token,
                                    )
                                    .await?
                                    .into_http_response()),
                                None => Err(trc::LimitEvent::SizeUpload.into_err()),
                            };
                        }
                    }
                    ("eventsource", &Method::GET) => {
                        // Authenticate request
                        let (_in_flight, access_token) =
                            self.authenticate_headers(&req, &session, false).await?;

                        return self.handle_event_source(req, access_token).await;
                    }
                    ("ws", &Method::GET) => {
                        // Authenticate request
                        let (_in_flight, access_token) =
                            self.authenticate_headers(&req, &session, false).await?;

                        return self
                            .upgrade_websocket_connection(req, access_token, session)
                            .await;
                    }
                    (_, &Method::OPTIONS) => {
                        return Ok(StatusCode::NO_CONTENT.into_http_response());
                    }
                    _ => (),
                }
            }
            ".well-known" => match (path.next().unwrap_or_default(), req.method()) {
                ("jmap", &Method::GET) => {
                    // Authenticate request
                    let (_in_flight, access_token) =
                        self.authenticate_headers(&req, &session, false).await?;

                    return self
                        .handle_session_resource(ctx.resolve_response_url(self).await, access_token)
                        .await
                        .map(|s| s.into_http_response());
                }
                ("oauth-authorization-server", &Method::GET) => {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self.handle_oauth_metadata(req, session).await;
                }
                ("openid-configuration", &Method::GET) => {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self.handle_oidc_metadata(req, session).await;
                }
                ("acme-challenge", &Method::GET) if self.has_acme_http_providers() => {
                    if let Some(token) = path.next() {
                        return match self
                            .core
                            .storage
                            .lookup
                            .key_get::<String>(KeyValue::<()>::build_key(KV_ACME, token))
                            .await?
                        {
                            Some(proof) => Ok(Resource::new("text/plain", proof.into_bytes())
                                .into_http_response()),
                            None => Err(trc::ResourceEvent::NotFound.into_err()),
                        };
                    }
                }
                ("mta-sts.txt", &Method::GET) => {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return if let Some(policy) = self.build_mta_sts_policy() {
                        Ok(Resource::new("text/plain", policy.to_string().into_bytes())
                            .into_http_response())
                    } else {
                        Err(trc::ResourceEvent::NotFound.into_err())
                    };
                }
                ("mail-v1.xml", &Method::GET) => {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self.handle_autoconfig_request(&req).await;
                }
                ("autoconfig", &Method::GET) => {
                    if path.next().unwrap_or_default() == "mail"
                        && path.next().unwrap_or_default() == "config-v1.1.xml"
                    {
                        // Limit anonymous requests
                        self.is_http_anonymous_request_allowed(&session.remote_ip)
                            .await?;

                        return self.handle_autoconfig_request(&req).await;
                    }
                }
                (_, &Method::OPTIONS) => {
                    return Ok(StatusCode::NO_CONTENT.into_http_response());
                }
                _ => (),
            },
            "auth" => match (path.next().unwrap_or_default(), req.method()) {
                ("device", &Method::POST) => {
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self.handle_device_auth(&mut req, session).await;
                }
                ("token", &Method::POST) => {
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self.handle_token_request(&mut req, session).await;
                }
                ("introspect", &Method::POST) => {
                    // Authenticate request
                    let (_in_flight, access_token) =
                        self.authenticate_headers(&req, &session, false).await?;

                    return self
                        .handle_token_introspect(&mut req, &access_token, session.session_id)
                        .await;
                }
                ("userinfo", &Method::GET) => {
                    // Authenticate request
                    let (_in_flight, access_token) =
                        self.authenticate_headers(&req, &session, false).await?;

                    return self.handle_userinfo_request(&access_token).await;
                }
                ("register", &Method::POST) => {
                    return self
                        .handle_oauth_registration_request(&mut req, session)
                        .await;
                }
                ("jwks.json", &Method::GET) => {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return Ok(self.core.oauth.oidc_jwks.clone().into_http_response());
                }
                (_, &Method::OPTIONS) => {
                    return Ok(StatusCode::NO_CONTENT.into_http_response());
                }
                _ => (),
            },
            "api" => {
                // Allow CORS preflight requests
                if req.method() == Method::OPTIONS {
                    return Ok(StatusCode::NO_CONTENT.into_http_response());
                }

                // Authenticate user
                match self.authenticate_headers(&req, &session, true).await {
                    Ok((_, access_token)) => {
                        return self
                            .handle_api_manage_request(&mut req, access_token, &session)
                            .await;
                    }
                    Err(err) => {
                        if err.matches(trc::EventType::Auth(trc::AuthEvent::Failed)) {
                            let params = UrlParams::new(req.uri().query());
                            let path = req.uri().path().split('/').skip(2).collect::<Vec<_>>();

                            let (grant_type, token) = match (
                                path.first().copied(),
                                path.get(1).copied(),
                                params.get("token"),
                            ) {
                                // SPDX-SnippetBegin
                                // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                                // SPDX-License-Identifier: LicenseRef-SEL
                                #[cfg(feature = "enterprise")]
                                (Some("telemetry"), Some("traces"), Some(token))
                                    if self.core.is_enterprise_edition() =>
                                {
                                    (GrantType::LiveTracing, token)
                                }
                                #[cfg(feature = "enterprise")]
                                (Some("telemetry"), Some("metrics"), Some(token))
                                    if self.core.is_enterprise_edition() =>
                                {
                                    (GrantType::LiveMetrics, token)
                                }
                                // SPDX-SnippetEnd
                                (Some("troubleshoot"), _, Some(token)) => {
                                    (GrantType::Troubleshoot, token)
                                }
                                _ => return Err(err),
                            };
                            let token_info =
                                self.validate_access_token(grant_type.into(), token).await?;

                            return match grant_type {
                                // SPDX-SnippetBegin
                                // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                                // SPDX-License-Identifier: LicenseRef-SEL
                                #[cfg(feature = "enterprise")]
                                GrantType::LiveTracing | GrantType::LiveMetrics => {
                                    self.handle_telemetry_api_request(
                                        &req,
                                        path,
                                        &AccessToken::from_id(token_info.account_id)
                                            .with_permission(Permission::MetricsLive)
                                            .with_permission(Permission::TracingLive),
                                    )
                                    .await
                                }
                                // SPDX-SnippetEnd
                                GrantType::Troubleshoot => {
                                    self.handle_troubleshoot_api_request(
                                        &req,
                                        path,
                                        &AccessToken::from_id(token_info.account_id)
                                            .with_permission(Permission::Troubleshoot),
                                        None,
                                    )
                                    .await
                                }
                                _ => unreachable!(),
                            };
                        }

                        return Err(err);
                    }
                }
            }
            "mail" => {
                if req.method() == Method::GET
                    && path.next().unwrap_or_default() == "config-v1.1.xml"
                {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self.handle_autoconfig_request(&req).await;
                }
            }
            "autodiscover" => {
                if req.method() == Method::POST
                    && path.next().unwrap_or_default() == "autodiscover.xml"
                {
                    // Limit anonymous requests
                    self.is_http_anonymous_request_allowed(&session.remote_ip)
                        .await?;

                    return self
                        .handle_autodiscover_request(
                            fetch_body(&mut req, 8192, session.session_id).await,
                        )
                        .await;
                }
            }
            "robots.txt" => {
                // Limit anonymous requests
                self.is_http_anonymous_request_allowed(&session.remote_ip)
                    .await?;

                return Ok(
                    Resource::new("text/plain", b"User-agent: *\nDisallow: /\n".to_vec())
                        .into_http_response(),
                );
            }
            "healthz" => {
                // Limit anonymous requests
                self.is_http_anonymous_request_allowed(&session.remote_ip)
                    .await?;

                match path.next().unwrap_or_default() {
                    "live" => {
                        return Ok(StatusCode::OK.into_http_response());
                    }
                    "ready" => {
                        return Ok({
                            if !self.core.storage.data.is_none() {
                                StatusCode::OK
                            } else {
                                StatusCode::SERVICE_UNAVAILABLE
                            }
                        }
                        .into_http_response());
                    }
                    _ => (),
                }
            }
            "metrics" => match path.next().unwrap_or_default() {
                "prometheus" => {
                    if let Some(prometheus) = &self.core.metrics.prometheus {
                        if let Some(auth) = &prometheus.auth {
                            if req
                                .authorization_basic()
                                .is_none_or( |secret| secret != auth)
                            {
                                return Err(trc::AuthEvent::Failed
                                    .into_err()
                                    .details("Invalid or missing credentials.")
                                    .caused_by(trc::location!()));
                            }
                        }

                        return Ok(Resource::new(
                            "text/plain; version=0.0.4",
                            self.export_prometheus_metrics().await?.into_bytes(),
                        )
                        .into_http_response());
                    }
                }
                "otel" => {
                    // Reserved for future use
                }
                _ => (),
            },
            #[cfg(feature = "enterprise")]
            "logo.svg" if self.is_enterprise_edition() => {
                // SPDX-SnippetBegin
                // SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
                // SPDX-License-Identifier: LicenseRef-SEL

                match self
                    .logo_resource(
                        req.headers()
                            .get(header::HOST)
                            .and_then(|h| h.to_str().ok())
                            .map(|h| h.rsplit_once(':').map_or(h, |(h, _)| h))
                            .unwrap_or_default(),
                    )
                    .await
                {
                    Ok(Some(resource)) => {
                        return Ok(resource.into_http_response());
                    }
                    Ok(None) => (),
                    Err(err) => {
                        trc::error!(err.span_id(session.session_id));
                    }
                }

                let resource = self.inner.data.webadmin.get("logo.svg").await?;

                if !resource.is_empty() {
                    return Ok(resource.into_http_response());
                }

                // SPDX-SnippetEnd
            }
            "form" => {
                if let Some(form) = &self.core.network.contact_form {
                    match *req.method() {
                        Method::POST => {
                            self.is_http_anonymous_request_allowed(&session.remote_ip)
                                .await?;

                            let form_data =
                                FormData::from_request(&mut req, form.max_size, session.session_id)
                                    .await?;

                            return self.handle_contact_form(&session, form, form_data).await;
                        }
                        Method::OPTIONS => {
                            return Ok(StatusCode::NO_CONTENT.into_http_response());
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                let path = req.uri().path();
                let resource = self
                    .inner
                    .data
                    .webadmin
                    .get(path.strip_prefix('/').unwrap_or(path))
                    .await?;

                if !resource.is_empty() {
                    return Ok(resource.into_http_response());
                }
            }
        }

        // Block dangerous URLs
        let path = req.uri().path();
        if self.is_http_banned_path(path, session.remote_ip).await? {
            trc::event!(
                Security(SecurityEvent::ScanBan),
                SpanId = session.session_id,
                RemoteIp = session.remote_ip,
                Path = path.to_string(),
            );
        }

        Err(trc::ResourceEvent::NotFound.into_err())
    }
}

async fn handle_session<T: SessionStream>(inner: Arc<Inner>, session: SessionData<T>) {
    let _in_flight = session.in_flight;
    let is_tls = session.stream.is_tls();

    if let Err(http_err) = http1::Builder::new()
        .keep_alive(true)
        .serve_connection(
            TokioIo::new(session.stream),
            service_fn(|req: hyper::Request<body::Incoming>| {
                let instance = session.instance.clone();
                let inner = inner.clone();

                async move {
                    let server = inner.build_server();

                    // Obtain remote IP
                    let remote_ip = if !server.core.jmap.http_use_forwarded {
                        trc::event!(
                            Http(trc::HttpEvent::RequestUrl),
                            SpanId = session.session_id,
                            Url = req.uri().to_string(),
                        );

                        session.remote_ip
                    } else if let Some(forwarded_for) = req
                        .headers()
                        .get(header::FORWARDED)
                        .and_then(|h| h.to_str().ok())
                        .and_then(|h| {
                            let h = h.to_ascii_lowercase();
                            h.split_once("for=").and_then(|(_, rest)| {
                                let mut start_ip = usize::MAX;
                                let mut end_ip = usize::MAX;

                                for (pos, ch) in rest.char_indices() {
                                    match ch {
                                        '0'..='9' | 'a'..='f' | ':' | '.' => {
                                            if start_ip == usize::MAX {
                                                start_ip = pos;
                                            }
                                            end_ip = pos;
                                        }
                                        '"' | '[' | ' ' if start_ip == usize::MAX => {}
                                        _ => {
                                            break;
                                        }
                                    }
                                }

                                rest.get(start_ip..=end_ip)
                                    .and_then(|h| h.parse::<IpAddr>().ok())
                            })
                        })
                        .or_else(|| {
                            req.headers()
                                .get("X-Forwarded-For")
                                .and_then(|h| h.to_str().ok())
                                .map(|h| h.split_once(',').map_or(h, |(ip, _)| ip).trim())
                                .and_then(|h| h.parse::<IpAddr>().ok())
                        })
                    {
                        // Check if the forwarded IP has been blocked
                        if server.is_ip_blocked(&forwarded_for) {
                            trc::event!(
                                Security(trc::SecurityEvent::IpBlocked),
                                ListenerId = instance.id.clone(),
                                RemoteIp = forwarded_for,
                                SpanId = session.session_id,
                            );

                            return Ok::<_, hyper::Error>(
                                StatusCode::FORBIDDEN.into_http_response().build(),
                            );
                        }

                        trc::event!(
                            Http(trc::HttpEvent::RequestUrl),
                            SpanId = session.session_id,
                            RemoteIp = forwarded_for,
                            Url = req.uri().to_string(),
                        );

                        forwarded_for
                    } else {
                        trc::event!(
                            Http(trc::HttpEvent::XForwardedMissing),
                            SpanId = session.session_id,
                        );
                        session.remote_ip
                    };

                    // Parse HTTP request
                    let response = match server
                        .parse_http_request(
                            req,
                            HttpSessionData {
                                instance,
                                local_ip: session.local_ip,
                                local_port: session.local_port,
                                remote_ip,
                                remote_port: session.remote_port,
                                is_tls,
                                session_id: session.session_id,
                            },
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(err) => {
                            let response = err.into_http_response();
                            trc::error!(err.span_id(session.session_id));
                            response
                        }
                    };

                    trc::event!(
                        Http(trc::HttpEvent::ResponseBody),
                        SpanId = session.session_id,
                        Contents = match &response.body {
                            HttpResponseBody::Text(value) => trc::Value::String(value.clone()),
                            HttpResponseBody::Binary(_) => trc::Value::Static("[binary data]"),
                            HttpResponseBody::Stream(_) => trc::Value::Static("[stream]"),
                            _ => trc::Value::None,
                        },
                        Code = response.status.as_u16(),
                        Size = response.size(),
                    );

                    // Build response
                    let mut response = response.build();

                    // Add custom headers
                    if !server.core.jmap.http_headers.is_empty() {
                        let headers = response.headers_mut();

                        for (header, value) in &server.core.jmap.http_headers {
                            headers.insert(header.clone(), value.clone());
                        }
                    }

                    Ok::<_, hyper::Error>(response)
                }
            }),
        )
        .with_upgrades()
        .await
    {
        match inner
            .build_server()
            .is_scanner_fail2banned(session.remote_ip)
            .await
        {
            Ok(true) => {
                trc::event!(
                    Security(SecurityEvent::ScanBan),
                    SpanId = session.session_id,
                    RemoteIp = session.remote_ip,
                    Reason = http_err.to_string(),
                );
            }
            Ok(false) => {
                trc::event!(
                    Http(trc::HttpEvent::Error),
                    SpanId = session.session_id,
                    Reason = http_err.to_string(),
                );
            }
            Err(err) => {
                trc::error!(err
                    .span_id(session.session_id)
                    .details("Failed to check for fail2ban"));
            }
        }
    }
}

impl SessionManager for JmapSessionManager {
    fn handle<T: SessionStream>(self, session: SessionData<T>) -> impl Future<Output = ()> + Send {
        handle_session(self.inner, session)
    }

    #[allow(clippy::manual_async_fn)]
    fn shutdown(&self) -> impl std::future::Future<Output = ()> + Send {
        async {
            let _ = self.inner.ipc.state_tx.send(StateEvent::Stop).await;
        }
    }
}

pub struct HttpContext<'x> {
    pub session: &'x HttpSessionData,
    pub req: &'x HttpRequest,
}

impl<'x> HttpContext<'x> {
    pub fn new(session: &'x HttpSessionData, req: &'x HttpRequest) -> Self {
        Self { session, req }
    }

    pub async fn resolve_response_url(&self, server: &Server) -> String {
        server
            .eval_if(
                &server.core.network.http_response_url,
                self,
                self.session.session_id,
            )
            .await
            .unwrap_or_else(|| {
                format!(
                    "http{}://{}:{}",
                    if self.session.is_tls { "s" } else { "" },
                    self.session.local_ip,
                    self.session.local_port
                )
            })
    }

    pub async fn has_endpoint_access(&self, server: &Server) -> StatusCode {
        server
            .eval_if(
                &server.core.network.http_allowed_endpoint,
                self,
                self.session.session_id,
            )
            .await
            .unwrap_or(StatusCode::OK)
    }
}

impl ResolveVariable for HttpContext<'_> {
    fn resolve_variable(&self, variable: u32) -> Variable<'_> {
        match variable {
            V_REMOTE_IP => self.session.remote_ip.to_string().into(),
            V_REMOTE_PORT => self.session.remote_port.into(),
            V_LOCAL_IP => self.session.local_ip.to_string().into(),
            V_LOCAL_PORT => self.session.local_port.into(),
            V_TLS => self.session.is_tls.into(),
            V_PROTOCOL => if self.session.is_tls { "https" } else { "http" }.into(),
            V_LISTENER => self.session.instance.id.as_str().into(),
            V_URL => self.req.uri().to_string().into(),
            V_URL_PATH => self.req.uri().path().into(),
            V_METHOD => self.req.method().as_str().into(),
            V_HEADERS => self
                .req
                .headers()
                .iter()
                .map(|(h, v)| {
                    Variable::String(
                        format!("{}: {}", h.as_str(), v.to_str().unwrap_or_default()).into(),
                    )
                })
                .collect::<Vec<_>>()
                .into(),
            _ => Variable::default(),
        }
    }

    fn resolve_global(&self, _: &str) -> Variable<'_> {
        Variable::Integer(0)
    }
}

pub async fn fetch_body(
    req: &mut HttpRequest,
    max_size: usize,
    session_id: u64,
) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(1024);
    while let Some(Ok(frame)) = req.frame().await {
        if let Some(data) = frame.data_ref() {
            if bytes.len() + data.len() <= max_size || max_size == 0 {
                bytes.extend_from_slice(data);
            } else {
                trc::event!(
                    Http(trc::HttpEvent::RequestBody),
                    SpanId = session_id,
                    Contents = std::str::from_utf8(&bytes)
                        .unwrap_or("[binary data]")
                        .to_string(),
                    Size = bytes.len(),
                    Limit = max_size,
                );

                return None;
            }
        }
    }

    trc::event!(
        Http(trc::HttpEvent::RequestBody),
        SpanId = session_id,
        Contents = std::str::from_utf8(&bytes)
            .unwrap_or("[binary data]")
            .to_string(),
        Size = bytes.len(),
    );

    bytes.into()
}

pub trait ToHttpResponse {
    fn into_http_response(self) -> HttpResponse;
}

impl HttpResponse {
    pub fn new_empty(status: StatusCode) -> Self {
        HttpResponse {
            status,
            content_type: "".into(),
            content_disposition: "".into(),
            cache_control: "".into(),
            body: HttpResponseBody::Empty,
        }
    }

    pub fn new_text(
        status: StatusCode,
        content_type: impl Into<Cow<'static, str>>,
        body: impl Into<String>,
    ) -> Self {
        HttpResponse {
            status,
            content_type: content_type.into(),
            content_disposition: "".into(),
            cache_control: "".into(),
            body: HttpResponseBody::Text(body.into()),
        }
    }

    pub fn new_binary(
        status: StatusCode,
        content_type: impl Into<Cow<'static, str>>,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        HttpResponse {
            status,
            content_type: content_type.into(),
            content_disposition: "".into(),
            cache_control: "".into(),
            body: HttpResponseBody::Binary(body.into()),
        }
    }

    pub fn size(&self) -> usize {
        match &self.body {
            HttpResponseBody::Text(value) => value.len(),
            HttpResponseBody::Binary(value) => value.len(),
            _ => 0,
        }
    }

    pub fn build(
        self,
    ) -> hyper::Response<http_body_util::combinators::BoxBody<hyper::body::Bytes, hyper::Error>>
    {
        let builder = hyper::Response::builder().status(self.status);

        match self.body {
            HttpResponseBody::Text(body) => builder
                .header(header::CONTENT_TYPE, self.content_type.as_ref())
                .body(
                    Full::new(Bytes::from(body))
                        .map_err(|never| match never {})
                        .boxed(),
                ),
            HttpResponseBody::Binary(body) => {
                let mut builder = builder.header(header::CONTENT_TYPE, self.content_type.as_ref());

                if !self.content_disposition.is_empty() {
                    builder = builder.header(
                        header::CONTENT_DISPOSITION,
                        self.content_disposition.as_ref(),
                    );
                }

                if !self.cache_control.is_empty() {
                    builder = builder.header(header::CACHE_CONTROL, self.cache_control.as_ref());
                }

                builder.body(
                    Full::new(Bytes::from(body))
                        .map_err(|never| match never {})
                        .boxed(),
                )
            }
            HttpResponseBody::Empty => builder.body(
                Full::new(Bytes::new())
                    .map_err(|never| match never {})
                    .boxed(),
            ),
            HttpResponseBody::Stream(stream) => builder
                .header(header::CONTENT_TYPE, self.content_type.as_ref())
                .header(header::CACHE_CONTROL, self.cache_control.as_ref())
                .body(stream),
            HttpResponseBody::WebsocketUpgrade(derived_key) => builder
                .header(header::CONNECTION, "upgrade")
                .header(header::UPGRADE, "websocket")
                .header("Sec-WebSocket-Accept", &derived_key)
                .header("Sec-WebSocket-Protocol", "jmap")
                .body(
                    Full::new(Bytes::from("Switching to WebSocket protocol"))
                        .map_err(|never| match never {})
                        .boxed(),
                ),
        }
        .unwrap()
    }
}

impl<T: serde::Serialize> ToHttpResponse for JsonResponse<T> {
    fn into_http_response(self) -> HttpResponse {
        HttpResponse {
            status: self.status,
            content_type: "application/json; charset=utf-8".into(),
            content_disposition: "".into(),
            cache_control: if !self.no_cache {
                ""
            } else {
                "no-store, no-cache, must-revalidate"
            }
            .into(),
            body: HttpResponseBody::Text(serde_json::to_string(&self.inner).unwrap_or_default()),
        }
    }
}

impl ToHttpResponse for &trc::Error {
    fn into_http_response(self) -> HttpResponse {
        match self.as_ref() {
            trc::EventType::Manage(cause) => {
                match cause {
                    trc::ManageEvent::MissingParameter => ManagementApiError::FieldMissing {
                        field: self.value_as_str(trc::Key::Key).unwrap_or_default(),
                    },
                    trc::ManageEvent::AlreadyExists => ManagementApiError::FieldAlreadyExists {
                        field: self.value_as_str(trc::Key::Key).unwrap_or_default(),
                        value: self.value_as_str(trc::Key::Value).unwrap_or_default(),
                    },
                    trc::ManageEvent::NotFound => ManagementApiError::NotFound {
                        item: self.value_as_str(trc::Key::Key).unwrap_or_default(),
                    },
                    trc::ManageEvent::NotSupported => ManagementApiError::Unsupported {
                        details: self
                            .value(trc::Key::Details)
                            .or_else(|| self.value(trc::Key::Reason))
                            .and_then(|v| v.as_str())
                            .unwrap_or("Requested action is unsupported"),
                    },
                    trc::ManageEvent::AssertFailed => ManagementApiError::AssertFailed,
                    trc::ManageEvent::Error => ManagementApiError::Other {
                        reason: self.value_as_str(trc::Key::Reason),
                        details: self
                            .value_as_str(trc::Key::Details)
                            .unwrap_or("Unknown error"),
                    },
                }
            }
            .into_http_response(),

            _ => self.to_request_error().into_http_response(),
        }
    }
}

pub trait ToRequestError {
    fn to_request_error(&self) -> RequestError<'_>;
}

impl ToRequestError for trc::Error {
    fn to_request_error(&self) -> RequestError<'_> {
        let details_or_reason = self
            .value(trc::Key::Details)
            .or_else(|| self.value(trc::Key::Reason))
            .and_then(|v| v.as_str());
        let details = details_or_reason.unwrap_or_else(|| self.as_ref().message());

        match self.as_ref() {
            trc::EventType::Jmap(cause) => match cause {
                trc::JmapEvent::UnknownCapability => RequestError::unknown_capability(details),
                trc::JmapEvent::NotJson => RequestError::not_json(details),
                trc::JmapEvent::NotRequest => RequestError::not_request(details),
                _ => RequestError::invalid_parameters(),
            },
            trc::EventType::Limit(cause) => match cause {
                trc::LimitEvent::SizeRequest => RequestError::limit(RequestLimitError::SizeRequest),
                trc::LimitEvent::SizeUpload => RequestError::limit(RequestLimitError::SizeUpload),
                trc::LimitEvent::CallsIn => RequestError::limit(RequestLimitError::CallsIn),
                trc::LimitEvent::ConcurrentRequest | trc::LimitEvent::ConcurrentConnection => {
                    RequestError::limit(RequestLimitError::ConcurrentRequest)
                }
                trc::LimitEvent::ConcurrentUpload => {
                    RequestError::limit(RequestLimitError::ConcurrentUpload)
                }
                trc::LimitEvent::Quota => RequestError::over_quota(),
                trc::LimitEvent::TenantQuota => RequestError::tenant_over_quota(),
                trc::LimitEvent::BlobQuota => RequestError::over_blob_quota(
                    self.value(trc::Key::Total)
                        .and_then(|v| v.to_uint())
                        .unwrap_or_default() as usize,
                    self.value(trc::Key::Size)
                        .and_then(|v| v.to_uint())
                        .unwrap_or_default() as usize,
                ),
                trc::LimitEvent::TooManyRequests => RequestError::too_many_requests(),
            },
            trc::EventType::Auth(cause) => match cause {
                trc::AuthEvent::MissingTotp => {
                    RequestError::blank(402, "TOTP code required", cause.message())
                }
                trc::AuthEvent::TooManyAttempts => RequestError::too_many_auth_attempts(),
                _ => RequestError::unauthorized(),
            },
            trc::EventType::Security(cause) => match cause {
                trc::SecurityEvent::AuthenticationBan
                | trc::SecurityEvent::ScanBan
                | trc::SecurityEvent::AbuseBan
                | trc::SecurityEvent::LoiterBan
                | trc::SecurityEvent::IpBlocked => RequestError::too_many_auth_attempts(),
                trc::SecurityEvent::Unauthorized => RequestError::forbidden(),
            },
            trc::EventType::Resource(cause) => match cause {
                trc::ResourceEvent::NotFound => RequestError::not_found(),
                trc::ResourceEvent::BadParameters => RequestError::blank(
                    StatusCode::BAD_REQUEST.as_u16(),
                    "Invalid parameters",
                    details_or_reason.unwrap_or("One or multiple parameters could not be parsed."),
                ),
                trc::ResourceEvent::Error => RequestError::internal_server_error(),
                _ => RequestError::internal_server_error(),
            },
            _ => RequestError::internal_server_error(),
        }
    }
}

impl<T: serde::Serialize> JsonResponse<T> {
    pub fn new(inner: T) -> Self {
        JsonResponse {
            inner,
            status: StatusCode::OK,
            no_cache: false,
        }
    }

    pub fn with_status(status: StatusCode, inner: T) -> Self {
        JsonResponse {
            inner,
            status,
            no_cache: false,
        }
    }

    pub fn no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }
}

impl HtmlResponse {
    pub fn new(body: String) -> Self {
        HtmlResponse {
            body,
            status: StatusCode::OK,
        }
    }

    pub fn with_status(status: StatusCode, body: String) -> Self {
        HtmlResponse { body, status }
    }
}

impl ToHttpResponse for Response {
    fn into_http_response(self) -> HttpResponse {
        JsonResponse::new(self).into_http_response()
    }
}

impl ToHttpResponse for Session {
    fn into_http_response(self) -> HttpResponse {
        JsonResponse::new(self).into_http_response()
    }
}

impl ToHttpResponse for ManagementApiError<'_> {
    fn into_http_response(self) -> super::HttpResponse {
        JsonResponse::new(self).into_http_response()
    }
}

impl ToHttpResponse for DownloadResponse {
    fn into_http_response(self) -> HttpResponse {
        HttpResponse {
            status: StatusCode::OK,
            content_type: self.content_type.into(),
            content_disposition: format!(
                "attachment; filename=\"{}\"",
                self.filename.replace('\"', "\\\"")
            )
            .into(),
            cache_control: "private, immutable, max-age=31536000".into(),
            body: HttpResponseBody::Binary(self.blob),
        }
    }
}

impl ToHttpResponse for Resource<Vec<u8>> {
    fn into_http_response(self) -> HttpResponse {
        HttpResponse::new_binary(StatusCode::OK, self.content_type, self.contents)
    }
}

impl ToHttpResponse for UploadResponse {
    fn into_http_response(self) -> HttpResponse {
        JsonResponse::new(self).into_http_response()
    }
}

impl ToHttpResponse for RequestError<'_> {
    fn into_http_response(self) -> HttpResponse {
        HttpResponse::new_text(
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_REQUEST),
            "application/problem+json",
            serde_json::to_string(&self).unwrap_or_default(),
        )
    }
}

impl ToHttpResponse for HtmlResponse {
    fn into_http_response(self) -> HttpResponse {
        HttpResponse::new_text(self.status, "text/html; charset=utf-8", self.body)
    }
}

impl ToHttpResponse for StatusCode {
    fn into_http_response(self) -> HttpResponse {
        HttpResponse::new_text(
            self,
            "application/problem+json",
            serde_json::to_string(&RequestError {
                p_type: jmap_proto::error::request::RequestErrorType::Other,
                status: self.as_u16(),
                title: None,
                detail: self.canonical_reason().unwrap_or_default().into(),
                limit: None,
            })
            .unwrap_or_default(),
        )
    }
}
