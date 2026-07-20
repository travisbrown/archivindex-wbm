//! A simplified HTTP status-code enumeration covering the values seen in CDX index results, with
//! several Cloudflare-specific codes.
use std::{fmt::Display, str::FromStr};

pub const STATUS_CODE_VALUES: [StatusCode; 25] = [
    StatusCode::Empty,
    StatusCode::Ok,
    StatusCode::MovedPermanently,
    StatusCode::Found,
    StatusCode::SeeOther,
    StatusCode::TemporaryRedirect,
    StatusCode::PermanentRedirect,
    StatusCode::BadRequest,
    StatusCode::Unauthorized,
    StatusCode::Forbidden,
    StatusCode::NotFound,
    StatusCode::RequestTimeout,
    StatusCode::UpgradeRequired,
    StatusCode::TooManyRequests,
    StatusCode::RequestHeaderFieldsTooLarge,
    StatusCode::InternalServerError,
    StatusCode::BadGateway,
    StatusCode::ServiceUnavailable,
    StatusCode::GatewayTimeout,
    StatusCode::CloudflareUnknownError,
    StatusCode::CloudflareWebServerDown,
    StatusCode::CloudflareConnectionTimeout,
    StatusCode::CloudflareTimeout,
    StatusCode::CloudflareSslHandshakeFailed,
    StatusCode::CloudflareOriginDnsError,
];

#[derive(thiserror::Error, Debug, Eq, PartialEq)]
pub enum Error {
    #[error("Unsupported status code")]
    Unsupported,
}

/// Generates [`StatusCode`] and its conversions from a single table, so the variant set lives in
/// exactly one place.
macro_rules! status_codes {
    ($($variant:ident => $code:literal / $str:literal),+ $(,)?) => {
        /// Represents an HTTP status code.
        ///
        /// This is a simplified representation that only provides coverage for values relevant to
        /// our CDX index results. The serialization encoding provided here is the one seen in these
        /// results.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize)]
        pub enum StatusCode {
            $(#[serde(alias = $str)] $variant),+
        }

        impl StatusCode {
            /// Returns the integer value of the status code.
            ///
            /// Note that this returns zero for an empty value, even though these typically indicate
            /// a `200` response. Use the `From` instance for `http::status::StatusCode` if you want
            /// a logical status code.
            #[must_use]
            pub const fn value(&self) -> u16 {
                match self {
                    $(Self::$variant => $code),+
                }
            }

            pub const fn from_value(value: u16) -> Result<Self, Error> {
                match value {
                    $($code => Ok(Self::$variant),)+
                    _ => Err(Error::Unsupported),
                }
            }

            /// The CDX string form of the status code (e.g. `"200"`, `"-"`).
            #[must_use]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $str),+
                }
            }
        }

        impl FromStr for StatusCode {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($str => Ok(Self::$variant),)+
                    _ => Err(Error::Unsupported),
                }
            }
        }
    };
}

status_codes! {
    Empty => 0 / "-",
    Ok => 200 / "200",
    MovedPermanently => 301 / "301",
    Found => 302 / "302",
    SeeOther => 303 / "303",
    TemporaryRedirect => 307 / "307",
    PermanentRedirect => 308 / "308",
    BadRequest => 400 / "400",
    Unauthorized => 401 / "401",
    Forbidden => 403 / "403",
    NotFound => 404 / "404",
    RequestTimeout => 408 / "408",
    UpgradeRequired => 426 / "426",
    TooManyRequests => 429 / "429",
    RequestHeaderFieldsTooLarge => 431 / "431",
    InternalServerError => 500 / "500",
    BadGateway => 502 / "502",
    ServiceUnavailable => 503 / "503",
    GatewayTimeout => 504 / "504",
    CloudflareUnknownError => 520 / "520",
    CloudflareWebServerDown => 521 / "521",
    CloudflareConnectionTimeout => 522 / "522",
    CloudflareTimeout => 524 / "524",
    CloudflareSslHandshakeFailed => 525 / "525",
    CloudflareOriginDnsError => 530 / "530",
}

impl Display for StatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Serializes as the CDX string form (e.g. `"200"`, `"-"`), matching [`Display`] and the form
/// accepted on deserialization.
impl serde::ser::Serialize for StatusCode {
    fn serialize<S: serde::ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl From<StatusCode> for http::status::StatusCode {
    /// Note that the Cloudflare error status codes are converted to the generic `500`.
    fn from(value: StatusCode) -> Self {
        match value {
            StatusCode::Ok | StatusCode::Empty => Self::OK,
            StatusCode::MovedPermanently => Self::MOVED_PERMANENTLY,
            StatusCode::Found => Self::FOUND,
            StatusCode::SeeOther => Self::SEE_OTHER,
            StatusCode::TemporaryRedirect => Self::TEMPORARY_REDIRECT,
            StatusCode::PermanentRedirect => Self::PERMANENT_REDIRECT,
            StatusCode::BadRequest => Self::BAD_REQUEST,
            StatusCode::Unauthorized => Self::UNAUTHORIZED,
            StatusCode::Forbidden => Self::FORBIDDEN,
            StatusCode::NotFound => Self::NOT_FOUND,
            StatusCode::RequestTimeout => Self::REQUEST_TIMEOUT,
            StatusCode::UpgradeRequired => Self::UPGRADE_REQUIRED,
            StatusCode::TooManyRequests => Self::TOO_MANY_REQUESTS,
            StatusCode::RequestHeaderFieldsTooLarge => Self::REQUEST_HEADER_FIELDS_TOO_LARGE,
            StatusCode::InternalServerError
            | StatusCode::CloudflareUnknownError
            | StatusCode::CloudflareWebServerDown
            | StatusCode::CloudflareConnectionTimeout
            | StatusCode::CloudflareTimeout
            | StatusCode::CloudflareSslHandshakeFailed
            | StatusCode::CloudflareOriginDnsError => Self::INTERNAL_SERVER_ERROR,
            StatusCode::BadGateway => Self::BAD_GATEWAY,
            StatusCode::ServiceUnavailable => Self::SERVICE_UNAVAILABLE,
            StatusCode::GatewayTimeout => Self::GATEWAY_TIMEOUT,
        }
    }
}

#[cfg(test)]
mod test {
    impl quickcheck::Arbitrary for super::StatusCode {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            // Safe because we know the slice is non-empty.
            *g.choose(&super::STATUS_CODE_VALUES).unwrap()
        }
    }

    #[test]
    fn ordering() {
        let mut values = super::STATUS_CODE_VALUES.to_vec();

        values.sort();
        assert_eq!(values, super::STATUS_CODE_VALUES);

        values.sort_by_key(super::StatusCode::value);
        assert_eq!(values, super::STATUS_CODE_VALUES);
    }

    #[test]
    fn round_trip_value() {
        for status_code in super::STATUS_CODE_VALUES {
            let status_code_value = status_code.value();
            let parsed = super::StatusCode::from_value(status_code_value);

            assert_eq!(parsed, Ok(status_code));
        }
    }

    #[test]
    fn round_trip_str() {
        for status_code in super::STATUS_CODE_VALUES {
            let status_code_str = status_code.to_string();
            let parsed = status_code_str.parse();

            assert_eq!(parsed, Ok(status_code));
        }
    }

    #[test]
    fn round_trip_json() {
        for status_code in super::STATUS_CODE_VALUES {
            let status_code_json = serde_json::json!(status_code);
            // Serialization uses the CDX string form (e.g. "200"), not the Rust variant name.
            assert_eq!(status_code_json, serde_json::json!(status_code.as_str()));

            let parsed: super::StatusCode =
                serde_json::from_str(&status_code_json.to_string()).unwrap();

            assert_eq!(parsed, status_code);
        }
    }
}
