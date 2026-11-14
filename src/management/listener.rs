//! Bind once, retaining the actual OS allocation and any restart handoff address.
use std::net::SocketAddr;

use tokio::net::TcpListener;

use super::ManagementError;
use crate::config::ManagementConfig;

const RESTART_LISTEN_ENV: &str = "BKMQB_INTERNAL_RESTART_LISTEN";

pub(super) async fn bind(config: &ManagementConfig) -> Result<TcpListener, ManagementError> {
    let preferred = if config.auto_port {
        match std::env::var(RESTART_LISTEN_ENV) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(ManagementError::InvalidRestartListen);
            }
        }
    } else {
        None
    };
    bind_with_preferred(config, preferred.as_deref()).await
}

async fn bind_with_preferred(
    config: &ManagementConfig,
    preferred: Option<&str>,
) -> Result<TcpListener, ManagementError> {
    let mut listen = config
        .listen
        .parse::<SocketAddr>()
        .map_err(|_| ManagementError::InvalidListen(config.listen.clone()))?;
    if !listen.ip().is_loopback() || (listen.port() == 0 && !config.auto_port) {
        return Err(ManagementError::InvalidListen(config.listen.clone()));
    }
    if config.auto_port {
        listen.set_port(0);
        if let Some(value) = preferred {
            // SocketAddr has no names or credentials; bound parsing and never echo raw input.
            let address = (value.len() <= 64)
                .then(|| value.parse::<SocketAddr>().ok())
                .flatten()
                .filter(|address| address.port() != 0 && address.ip() == listen.ip())
                .ok_or(ManagementError::InvalidRestartListen)?;
            listen = address;
        }
    }
    TcpListener::bind(listen)
        .await
        .map_err(|source| ManagementError::Bind { listen, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn automatic(listen: &str) -> ManagementConfig {
        ManagementConfig {
            listen: listen.to_owned(),
            auto_port: true,
            ..ManagementConfig::default()
        }
    }

    #[tokio::test]
    async fn automatic_allocation_keeps_both_ipv4_instances_bound_and_distinct() {
        let config = automatic("127.0.0.1:52743");
        let first = bind_with_preferred(&config, None).await.unwrap();
        let second = bind_with_preferred(&config, None).await.unwrap();
        let address = first.local_addr().unwrap();
        assert_eq!(
            address.ip(),
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        );
        assert_ne!(address.port(), 0);
        assert_ne!(address, second.local_addr().unwrap());
    }

    #[tokio::test]
    async fn automatic_ipv6_when_supported() {
        let config = automatic("[::1]:0");
        let first = match bind_with_preferred(&config, None).await {
            Ok(listener) => listener,
            Err(ManagementError::Bind { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ) || source.raw_os_error() == Some(libc::EAFNOSUPPORT) =>
            {
                return;
            }
            Err(error) => panic!("unexpected IPv6 bind error: {error}"),
        };
        let second = bind_with_preferred(&config, None).await.unwrap();
        assert!(first.local_addr().unwrap().ip().is_loopback());
        assert_ne!(first.local_addr().unwrap().port(), 0);
        assert_ne!(first.local_addr().unwrap(), second.local_addr().unwrap());
    }

    #[tokio::test]
    async fn preferred_port_is_reused_or_fails_without_silent_fallback() {
        let config = automatic("127.0.0.1:0");
        let first = bind_with_preferred(&config, None).await.unwrap();
        let address = first.local_addr().unwrap();
        let preferred = address.to_string();
        assert!(
            matches!(bind_with_preferred(&config, Some(&preferred)).await,
            Err(ManagementError::Bind { listen, .. }) if listen == address)
        );
        drop(first);
        let restarted = bind_with_preferred(&config, Some(&preferred))
            .await
            .unwrap();
        assert_eq!(restarted.local_addr().unwrap(), address);
        let fixed = ManagementConfig {
            listen: preferred,
            ..ManagementConfig::default()
        };
        assert!(
            matches!(bind_with_preferred(&fixed, Some("invalid ignored")).await,
            Err(ManagementError::Bind { listen, .. }) if listen == address)
        );
        drop(restarted);
        assert_eq!(
            bind_with_preferred(&fixed, Some("invalid ignored"))
                .await
                .unwrap()
                .local_addr()
                .unwrap(),
            address
        );
    }

    #[tokio::test]
    async fn rejects_invalid_handoff_and_unsafe_config() {
        let config = automatic("127.0.0.1:0");
        for value in [
            "",
            "localhost:12345",
            "127.0.0.1:0",
            "127.0.0.2:12345",
            "[::1]:12345",
            "0.0.0.0:12345",
            &"a".repeat(65),
        ] {
            assert!(matches!(
                bind_with_preferred(&config, Some(value)).await,
                Err(ManagementError::InvalidRestartListen)
            ));
        }
        for auto_port in [false, true] {
            let config = ManagementConfig {
                listen: "0.0.0.0:12345".to_owned(),
                auto_port,
                ..ManagementConfig::default()
            };
            assert!(matches!(
                bind_with_preferred(&config, None).await,
                Err(ManagementError::InvalidListen(_))
            ));
        }
        let fixed = ManagementConfig {
            listen: "127.0.0.1:0".to_owned(),
            ..ManagementConfig::default()
        };
        assert!(matches!(
            bind_with_preferred(&fixed, None).await,
            Err(ManagementError::InvalidListen(_))
        ));
    }
}
