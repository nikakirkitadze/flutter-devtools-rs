//! Parsing of the VM service address a user pastes.

use url::Url;

/// A validated VM service address, normalised to its WebSocket form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmServiceUri {
    ws: Url,
    host_port: String,
    /// Path segments before the trailing `ws`.
    prefix_segments: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UriError {
    Empty,
    NotVmService,
    Secure,
}

impl UriError {
    pub fn message(&self) -> &'static str {
        match self {
            UriError::Empty => "Paste a VM service URL, for example http://127.0.0.1:50300/abc=/",
            UriError::NotVmService => {
                "That doesn't look like a VM service URL. It looks like http://127.0.0.1:50300/abc=/ or ws://127.0.0.1:50300/abc=/ws"
            }
            UriError::Secure => "Secure (https or wss) VM service URLs aren't supported yet.",
        }
    }
}

impl std::fmt::Display for UriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for UriError {}

impl VmServiceUri {
    pub fn parse(input: &str) -> Result<VmServiceUri, UriError> {
        Self::parse_depth(input, 0)
    }

    fn parse_depth(input: &str, depth: u8) -> Result<VmServiceUri, UriError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(UriError::Empty);
        }
        let with_scheme;
        let input = if input.contains("://") {
            input
        } else if input.contains(':') {
            with_scheme = format!("http://{input}");
            &with_scheme
        } else {
            return Err(UriError::NotVmService);
        };
        let url = Url::parse(input).map_err(|_| UriError::NotVmService)?;
        match url.scheme() {
            "http" | "ws" => {}
            "https" | "wss" => return Err(UriError::Secure),
            _ => return Err(UriError::NotVmService),
        }
        // A DevTools link carries the real address in `?uri=`.
        if depth == 0
            && let Some((_, inner)) = url.query_pairs().find(|(k, _)| k == "uri")
        {
            return Self::parse_depth(&inner, 1);
        }
        let host = url.host_str().ok_or(UriError::NotVmService)?;
        let port = url.port_or_known_default().ok_or(UriError::NotVmService)?;
        let mut segs: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
        if segs.last() == Some(&"ws") {
            segs.pop();
        }
        let mut path = String::new();
        for s in &segs {
            path.push('/');
            path.push_str(s);
        }
        path.push_str("/ws");
        let ws =
            Url::parse(&format!("ws://{host}:{port}{path}")).map_err(|_| UriError::NotVmService)?;
        Ok(VmServiceUri {
            ws,
            host_port: format!("{host}:{port}"),
            prefix_segments: segs.len(),
        })
    }

    pub fn ws_url(&self) -> &Url {
        &self.ws
    }

    pub fn host_port(&self) -> String {
        self.host_port.clone()
    }

    /// The WebSocket URL with the token segment replaced by `…`, safe to show and log.
    pub fn redacted(&self) -> String {
        if self.prefix_segments == 0 {
            format!("ws://{}/ws", self.host_port)
        } else {
            format!("ws://{}/…/ws", self.host_port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_parse_cases() {
        let ok = [
            (
                "http://127.0.0.1:50300/abc=/",
                "ws://127.0.0.1:50300/abc=/ws",
            ),
            (
                "http://127.0.0.1:50300/abc=",
                "ws://127.0.0.1:50300/abc=/ws",
            ),
            (
                "  ws://127.0.0.1:50300/abc=/ws \n",
                "ws://127.0.0.1:50300/abc=/ws",
            ),
            (
                "ws://127.0.0.1:50300/abc=/ws/",
                "ws://127.0.0.1:50300/abc=/ws",
            ),
            (
                "http://127.0.0.1:9100/?uri=http://127.0.0.1:50300/abc=/",
                "ws://127.0.0.1:50300/abc=/ws",
            ),
            (
                "http://127.0.0.1:9100/?uri=ws%3A%2F%2F127.0.0.1%3A50300%2Fabc%3D%2Fws",
                "ws://127.0.0.1:50300/abc=/ws",
            ),
            ("127.0.0.1:50300/abc=/", "ws://127.0.0.1:50300/abc=/ws"),
            ("http://localhost:50300/", "ws://localhost:50300/ws"),
        ];
        for (input, want) in ok {
            let got = VmServiceUri::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e:?}"));
            assert_eq!(got.ws_url().as_str(), want, "input {input:?}");
        }
        let bad = [
            ("https://h:443/abc=/", UriError::Secure),
            ("wss://h:443/abc=/ws", UriError::Secure),
            ("", UriError::Empty),
            ("   ", UriError::Empty),
            ("hello", UriError::NotVmService),
            ("ftp://x/", UriError::NotVmService),
        ];
        for (input, want) in bad {
            assert_eq!(
                VmServiceUri::parse(input).unwrap_err(),
                want,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn host_port_and_redaction() {
        let u = VmServiceUri::parse("http://127.0.0.1:50300/abc=/").unwrap();
        assert_eq!(u.host_port(), "127.0.0.1:50300");
        assert_eq!(u.redacted(), "ws://127.0.0.1:50300/…/ws");
        let u = VmServiceUri::parse("http://localhost:50300/").unwrap();
        assert_eq!(u.redacted(), "ws://localhost:50300/ws");
    }

    #[test]
    fn error_copy() {
        assert_eq!(
            UriError::Empty.message(),
            "Paste a VM service URL, for example http://127.0.0.1:50300/abc=/"
        );
        assert_eq!(
            UriError::Secure.message(),
            "Secure (https or wss) VM service URLs aren't supported yet."
        );
        assert!(
            UriError::NotVmService
                .message()
                .starts_with("That doesn't look like")
        );
    }
}
