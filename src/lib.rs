//! # rocket_oauth2
//!
//! OAuth2 ([RFC 6749](https://tools.ietf.org/html/rfc6749)) for
//! [Rocket](https://rocket.rs) applications.
//!
//! ## Requirements
//!
//! * Rocket 0.4
//!
//! ## API Stability
//!
//! `rocket_oauth2` is still in its early stages and the API is subject to heavy
//! change in the future. semver is respsected, but only the latest release will
//! be actively maintained.
//!
//! ## Features
//!
//! * Handles the Authorization Code Grant (RFC 6749, §4.1)
//! * Built-in support for a few popular OAuth2 providers
//! * Support for custom providers
//! * Support for custom adapters
//! * Refreshing tokens
//!
//! ## Not-yet-planned Features
//!
//! * Grant types other than Authorization Code.
//!
//! ## Overview
//!
//! This crate provides two request guards: [`OAuth2`] and [`TokenResponse`].
//! `OAuth2` is used to generate redirects to to authentication providers, and
//! `TokenResponse` is employed on the application's Redirect URI route to
//! complete the token exchange.
//!
//! The [`Adapter`] trait defines how the temporary code from the authorization
//! server is exchanged for an authentication token. `rocket_oauth2` currently
//! provides only one `Adapter`, using
//! [`hyper-sync-rustls`](https://github.com/SergioBenitez/hyper-sync-rustls).
//!
//! If necessary a custom `Adapter` can be used, for example to work around
//! a noncompliant authorization server.
//!
//! ## Usage
//!
//! Configure your OAuth client settings in `Rocket.toml`:
//! ```toml
//! [global.oauth.github]
//! provider = "GitHub"
//! client_id = "..."
//! client_secret = "..."
//! redirect_uri = "http://localhost:8000/auth/github"
//! ```
//!
//! Implement routes for a login URI and a redirect URI. Mount these routes
//! and attach the [OAuth2 Fairing](OAuth2::fairing()):
//!
//! ```rust,no_run
//! # #![feature(proc_macro_hygiene, decl_macro)]
//! # #[macro_use] extern crate rocket;
//! # extern crate rocket_oauth2;
//! # use rocket::http::{Cookie, Cookies, SameSite};
//! # use rocket::Request;
//! # use rocket::response::Redirect;
//! use rocket_oauth2::{OAuth2, TokenResponse};
//!
//! // This struct will only be used as a type-level key. Multiple
//! // instances of OAuth2 can be used in the same application by
//! // using different key types.
//! struct GitHub;
//!
//! // This route calls `get_redirect`, which sets up a token request and
//! // returns a `Redirect` to the authorization endpoint.
//! #[get("/login/github")]
//! fn github_login(oauth2: OAuth2<GitHub>, mut cookies: Cookies<'_>) -> Redirect {
//!     oauth2.get_redirect(&mut cookies, &["user:read"]).unwrap()
//! }
//!
//! // This route, mounted at the application's Redirect URI, uses the
//! // `TokenResponse` request guard to complete the token exchange and obtain
//! // the token.
//! #[get("/auth/github")]
//! fn github_callback(token: TokenResponse<GitHub>, mut cookies: Cookies<'_>) -> Redirect
//! {
//!     // Set a private cookie with the access token
//!     cookies.add_private(
//!         Cookie::build("token", token.access_token().to_string())
//!             .same_site(SameSite::Lax)
//!             .finish()
//!     );
//!     Redirect::to("/")
//! }
//!
//! fn main() {
//!     rocket::ignite()
//!         .mount("/", routes![github_callback, github_login])
//!         // The string "github" here matches [global.oauth2.github] in `Rocket.toml`
//!         .attach(OAuth2::<GitHub>::fairing("github"))
//!         .launch();
//! }
//! ```
//!
//! ### Provider selection
//!
//! Providers can be specified as a known provider name (case-insensitive).  The
//! known provider names are listed as associated constants on the
//! [`StaticProvider`] type.
//!
//! ```toml
//! [global.oauth.github]
//! # Using a known provider name
//! provider = "GitHub"
//! client_id = "..."
//! client_secret = "..."
//! redirect_uri = "http://localhost:8000/auth/github"
//! ```
//!
//! The provider can also be specified as a table with `auth_uri` and
//! `token_uri` values:
//!
//! ```toml
//! [global.oauth.custom]
//! provider = { auth_uri = "https://example.com/oauth/authorize", token_uri = "https://example.com/oauth/token" }
//! client_id = "..."
//! client_secret = "..."
//! redirect_uri = "http://localhost:8000/auth/custom"
//! ```

#![warn(future_incompatible, nonstandard_style, missing_docs)]

mod config;
mod error;

#[cfg(feature = "hyper_sync_rustls_adapter")]
mod hyper_sync_rustls_adapter;
#[cfg(feature = "hyper_sync_rustls_adapter")]
pub use hyper_sync_rustls_adapter::HyperSyncRustlsAdapter;

pub use self::config::*;
pub use self::error::*;

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use log::warn;
use rocket::fairing::{AdHoc, Fairing};
use rocket::http::uri::Absolute;
use rocket::http::{Cookie, Cookies, SameSite, Status};
use rocket::request::{self, FormItems, FromForm, FromRequest, Request};
use rocket::response::Redirect;
use rocket::{Outcome, State};
use serde_json::Value;

const STATE_COOKIE_NAME: &str = "rocket_oauth2_state";

// Random generation of state for defense against CSRF.
// See RFC 6749 §10.12 for more details.
fn generate_state(rng: &mut impl rand::RngCore) -> Result<String, Error> {
    let mut buf = [0; 16]; // 128 bits
    rng.try_fill_bytes(&mut buf).map_err(|_| {
        Error::new_from(
            ErrorKind::Other,
            String::from("Failed to generate random data"),
        )
    })?;
    Ok(base64::encode_config(&buf, base64::URL_SAFE_NO_PAD))
}

/// The token types which can be exchanged with the token endpoint
#[derive(Clone, PartialEq, Debug)]
pub enum TokenRequest {
    /// Used for the Authorization Code exchange
    AuthorizationCode(String),
    /// Used to refresh an access token
    RefreshToken(String),
}

/// The server's response to a successful token exchange, defined in
/// in RFC 6749 §5.1.
///
/// `TokenResponse<K>` implements `FromRequest`, and is used in the callback
/// route to complete the token exchange.
#[derive(Clone, PartialEq, Debug)]
pub struct TokenResponse<K> {
    data: Value,
    _k: PhantomData<fn() -> K>,
}

impl<K> TokenResponse<K> {
    /// Reinterpret this `TokenResponse` as if it were keyed by `L` instead.
    /// This function can be used to treat disparate `TokenResponse`s as a
    /// single concrete type such as `TokenResponse<()>` to avoid an explosion
    /// of generic bounds.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct GitHub;
    ///
    /// fn use_nongeneric_token(token: TokenResponse<()>) {
    ///     // ...
    /// }
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<GitHub>) {
    ///     use_nongeneric_token(token.cast());
    /// }
    /// ```
    pub fn cast<L>(self) -> TokenResponse<L> {
        TokenResponse {
            data: self.data,
            _k: PhantomData,
        }
    }

    /// Get the TokenResponse data as a raw JSON [Value]. It is guaranteed to
    /// be of type Object.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct MyProvider;
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<MyProvider>) {
    ///     let custom_data = token.as_value().get("custom_data").unwrap().as_str();
    /// }
    /// ```
    pub fn as_value(&self) -> &Value {
        &self.data
    }

    /// Get the access token issued by the authorization server.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<GitHub>) {
    ///     let access_token = token.access_token();
    /// }
    /// ```
    pub fn access_token(&self) -> &str {
        self.data
            .get("access_token")
            .and_then(Value::as_str)
            .expect("access_token required at construction")
    }

    /// Get the type of token, described in RFC 6749 §7.1.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<GitHub>) {
    ///     let token_type = token.token_type();
    /// }
    /// ```
    pub fn token_type(&self) -> &str {
        self.data
            .get("token_type")
            .and_then(Value::as_str)
            .expect("token_type required at construction")
    }

    /// Get the lifetime in seconds of the access token, if the authorization server provided one.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<GitHub>) {
    ///     if let Some(expires_in) = token.expires_in() {
    ///         println!("Token expires in {} seconds", expires_in);
    ///     }
    /// }
    /// ```
    pub fn expires_in(&self) -> Option<i64> {
        self.data.get("expires_in").and_then(Value::as_i64)
    }

    /// Get the refresh token, if the server provided one.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<GitHub>) {
    ///     if let Some(refresh_token) = token.refresh_token() {
    ///         println!("Got a refresh token! '{}'", refresh_token);
    ///     }
    /// }
    /// ```
    pub fn refresh_token(&self) -> Option<&str> {
        self.data.get("refresh_token").and_then(Value::as_str)
    }

    /// Get the (space-separated) list of scopes associated with the access
    /// token.  The authorization server is required to provide this if it
    /// differs from the requested set of scopes.
    ///
    /// If `scope` was not provided by the server as a string, this method will
    /// return `None`. For those providers, use `.as_value().get("scope")
    /// instead.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::TokenResponse;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/login/github")]
    /// fn login_github(token: TokenResponse<GitHub>) {
    ///     if let Some(scope) = token.scope() {
    ///         println!("Token scope: '{}'", scope);
    ///     }
    /// }
    /// ```
    pub fn scope(&self) -> Option<&str> {
        self.data.get("scope").and_then(Value::as_str)
    }
}

impl std::convert::TryFrom<Value> for TokenResponse<()> {
    type Error = Error;

    /// Construct a TokenResponse from a [Value].
    ///
    /// Returns an [Error] if data is not a JSON Object, or the access_token or token_type is
    /// missing or not a string.
    fn try_from(data: Value) -> Result<Self, Error> {
        if !data.is_object() {
            return Err(Error::new_from(
                ErrorKind::ExchangeFailure,
                String::from("TokenResponse data was not an object"),
            ));
        }
        match data.get("access_token") {
            Some(val) if val.is_string() => (),
            _ => {
                return Err(Error::new_from(
                    ErrorKind::ExchangeFailure,
                    String::from("TokenResponse access_token was missing or not a string"),
                ))
            }
        }
        match data.get("token_type") {
            Some(val) if val.is_string() => (),
            _ => {
                return Err(Error::new_from(
                    ErrorKind::ExchangeFailure,
                    String::from("TokenResponse token_type was missing or not a string"),
                ))
            }
        }

        Ok(Self {
            data,
            _k: PhantomData,
        })
    }
}

impl<'a, 'r, K: 'static> FromRequest<'a, 'r> for TokenResponse<K> {
    type Error = Error;

    // TODO: Decide if BadRequest is the appropriate error code.
    // TODO: What do providers do if they *reject* the authorization?
    /// Handle the redirect callback, delegating to the Adapter to perform the
    /// token exchange.
    fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, Self::Error> {
        let oauth2 = request
            .guard::<State<Arc<Shared<K>>>>()
            .expect("OAuth2 fairing was not attached for this key type!")
            .inner();

        // Parse the query data.
        let query = match request.uri().query() {
            Some(q) => q,
            None => {
                return Outcome::Failure((
                    Status::BadRequest,
                    Error::new_from(
                        ErrorKind::ExchangeFailure,
                        "Missing query string in request",
                    ),
                ))
            }
        };

        #[derive(FromForm)]
        struct CallbackQuery {
            code: String,
            state: String,
            // Nonstandard (but see below)
            scope: Option<String>,
        }

        let params = match CallbackQuery::from_form(&mut FormItems::from(query), false) {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to parse OAuth2 query string: {:?}", e);
                return Outcome::Failure((
                    Status::BadRequest,
                    Error::new_from(ErrorKind::ExchangeFailure, format!("{:?}", e)),
                ))
            }
        };

        {
            // Verify that the given state is the same one in the cookie.
            // Begin a new scope so that cookies is not kept around too long.
            let mut cookies = request.guard::<Cookies<'_>>().expect("request cookies");
            match cookies.get_private(STATE_COOKIE_NAME) {
                Some(ref cookie) if cookie.value() == params.state => {
                    cookies.remove(cookie.clone());
                }
                _ => {
                    warn!("The OAuth2 state returned from the server did not match the stored state.");
                    return Outcome::Failure((
                        Status::BadRequest,
                        Error::new_from(
                            ErrorKind::ExchangeFailure,
                            "The OAuth2 state returned from the server did match the stored state.",
                        ),
                    ))
                }
            }
        }
       
        // Have the adapter perform the token exchange.
        match oauth2
            .adapter
            .exchange_code(&oauth2.config, TokenRequest::AuthorizationCode(params.code))
        {
            Ok(mut token) => {
                // Some providers (at least Strava) provide 'scope' in the callback
                // parameters instead of the token response as the RFC prescribes.
                // Therefore the 'scope' from the callback params is used as a fallback
                // if the token response does not specify one.
                let data = token
                    .data
                    .as_object_mut()
                    .expect("data is guaranteed to be an Object");
                if let (None, Some(scope)) = (data.get("scope"), params.scope) {
                    data.insert(String::from("scope"), Value::String(scope));
                }
                Outcome::Success(token.cast())
            }
            Err(e) => {
                warn!("OAuth2 token exchange failed: {}", e);
                Outcome::Failure((Status::BadRequest, e))
            }
        }
    }
}

/// An OAuth2 `Adapater` can be implemented by any type that facilitates the
/// Authorization Code Grant as described in RFC 6749 §4.1. The implementing
/// type must be able to generate an authorization URI and perform the token
/// exchange.
pub trait Adapter: Send + Sync + 'static {
    /// Generate an authorization URI as described by RFC 6749 §4.1.1
    /// given configuration, state, and scopes.
    fn authorization_uri(
        &self,
        config: &OAuthConfig,
        state: &str,
        scopes: &[&str],
        duration: &str,
    ) -> Result<Absolute<'static>, Error>;

    /// Perform the token exchange in accordance with RFC 6749 §4.1.3 given the
    /// authorization code provided by the service.
    fn exchange_code(
        &self,
        config: &OAuthConfig,
        token: TokenRequest,
    ) -> Result<TokenResponse<()>, Error>;
}

struct Shared<K> {
    adapter: Box<dyn Adapter>,
    config: OAuthConfig,
    _k: PhantomData<fn() -> TokenResponse<K>>,
}

/// Utilities for OAuth authentication in Rocket applications.
pub struct OAuth2<K>(Arc<Shared<K>>);

impl<K: 'static> OAuth2<K> {
    /// Create an OAuth2 fairing. The fairing will read the configuration in
    /// `config_name` and register itself in the application so that
    /// `TokenResponse<K>` can be used.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use rocket::fairing::AdHoc;
    /// use rocket_oauth2::{HyperSyncRustlsAdapter, OAuth2, OAuthConfig};
    ///
    /// struct GitHub;
    ///
    /// fn main() {
    ///     rocket::ignite()
    ///         .attach(OAuth2::<GitHub>::fairing("github"))
    ///         .launch();
    /// }
    #[cfg(feature = "hyper_sync_rustls_adapter")]
    pub fn fairing(config_name: &str) -> impl Fairing {
        // Unfortunate allocations, but necessary because on_attach requires 'static
        let config_name = config_name.to_string();

        AdHoc::on_attach("OAuth Init", move |rocket| {
            let config = match OAuthConfig::from_config(rocket.config(), &config_name) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Invalid configuration: {:?}", e);
                    return Err(rocket);
                }
            };

            Ok(rocket.attach(Self::custom(
                hyper_sync_rustls_adapter::HyperSyncRustlsAdapter,
                config,
            )))
        })
    }

    /// Create an OAuth2 fairing with a custom adapter and configuration.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use rocket::fairing::AdHoc;
    /// use rocket_oauth2::{HyperSyncRustlsAdapter, OAuth2, OAuthConfig, StaticProvider};
    ///
    /// struct MyProvider;
    ///
    /// fn main() {
    ///     rocket::ignite()
    ///         .attach(AdHoc::on_attach("OAuth Config", |rocket| {
    ///             let config = OAuthConfig::new(
    ///                 StaticProvider {
    ///                     auth_uri: "auth uri".into(),
    ///                     token_uri: "token uri".into(),
    ///                 },
    ///                 "client id".to_string(),
    ///                 "client secret".to_string(),
    ///                 Some("http://localhost:8000/auth".to_string()),
    ///             );
    ///             Ok(rocket.attach(OAuth2::<MyProvider>::custom(HyperSyncRustlsAdapter, config)))
    ///         }))
    ///         .launch();
    /// }
    pub fn custom<A: Adapter>(adapter: A, config: OAuthConfig) -> impl Fairing {
        let shared = Shared::<K> {
            adapter: Box::new(adapter),
            config,
            _k: PhantomData,
        };

        AdHoc::on_attach("OAuth Mount", |rocket| Ok(rocket.manage(Arc::new(shared))))
    }

    /// Prepare an authentication redirect. This sets a state cookie and returns
    /// a `Redirect` to the authorization endpoint.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket::http::Cookies;
    /// use rocket::response::Redirect;
    /// use rocket_oauth2::OAuth2;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/login/github")]
    /// fn github_login(oauth2: OAuth2<GitHub>, mut cookies: Cookies<'_>) -> Redirect {
    ///     oauth2.get_redirect(&mut cookies, &["user:read"]).unwrap()
    /// }
    /// ```
    pub fn get_redirect(
        &self,
        cookies: &mut Cookies<'_>,
        scopes: &[&str],
    ) -> Result<Redirect, Error> {
        let state = generate_state(&mut rand::thread_rng())?;
        let uri = self
            .0
            .adapter
            .authorization_uri(&self.0.config, &state, scopes, "permanent")?;
        cookies.add_private(
            Cookie::build(STATE_COOKIE_NAME, state)
                .same_site(SameSite::Lax)
                .finish(),
        );
        Ok(Redirect::to(uri))
    }

    /// Request a new access token given a refresh token. The refresh token
    /// must have been returned by the provider in a previous [`TokenResponse`].
    ///
    /// # Example
    ///
    /// ```rust
    /// # #![feature(decl_macro)]
    /// use rocket_oauth2::OAuth2;
    ///
    /// struct GitHub;
    ///
    /// #[rocket::get("/")]
    /// fn index(oauth2: OAuth2<GitHub>) {
    ///     // get previously stored refresh_token
    ///     # let refresh_token = "";
    ///     oauth2.refresh(refresh_token).unwrap();
    /// }
    /// ```
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenResponse<K>, Error> {
        self.0
            .adapter
            .exchange_code(
                &self.0.config,
                TokenRequest::RefreshToken(refresh_token.to_string()),
            )
            .map(TokenResponse::cast)
    }
}

impl<'a, 'r, K: 'static> FromRequest<'a, 'r> for OAuth2<K> {
    type Error = ();

    fn from_request(request: &'a Request<'r>) -> request::Outcome<Self, Self::Error> {
        Outcome::Success(OAuth2(
            request
                .guard::<State<Arc<Shared<K>>>>()
                .expect("OAuth2 fairing was not attached for this key type!")
                .clone(),
        ))
    }
}

impl<C: fmt::Debug> fmt::Debug for OAuth2<C> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("OAuth2")
            .field("adapter", &(..))
            .field("config", &self.0.config)
            .finish()
    }
}
