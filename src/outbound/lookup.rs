use std::net::IpAddr;

use mail_auth::MX;
use rand::{seq::SliceRandom, Rng};

use crate::{
    core::{Core, Envelope},
    queue::{Error, ErrorDetails, Status},
};

use super::RemoteHost;

impl Core {
    pub(super) async fn resolve_host(
        &self,
        remote_host: &RemoteHost<'_>,
        envelope: &impl Envelope,
        max_multihomed: usize,
    ) -> Result<(Option<IpAddr>, Vec<IpAddr>), Status<(), Error>> {
        let remote_ips = self
            .resolvers
            .dns
            .ip_lookup(
                remote_host.fqdn_hostname().as_ref(),
                *self.queue.config.ip_strategy.eval(envelope).await,
                max_multihomed,
            )
            .await
            .map_err(|err| {
                if let mail_auth::Error::DnsRecordNotFound(_) = &err {
                    Status::PermanentFailure(Error::ConnectionError(ErrorDetails {
                        entity: remote_host.hostname().to_string(),
                        details: "record not found for MX".to_string(),
                    }))
                } else {
                    Status::TemporaryFailure(Error::ConnectionError(ErrorDetails {
                        entity: remote_host.hostname().to_string(),
                        details: format!("lookup error: {err}"),
                    }))
                }
            })?;

        if let Some(remote_ip) = remote_ips.first() {
            let mut source_ip = None;

            if remote_ip.is_ipv4() {
                let source_ips = self.queue.config.source_ip.ipv4.eval(envelope).await;
                match source_ips.len().cmp(&1) {
                    std::cmp::Ordering::Equal => {
                        source_ip = IpAddr::from(*source_ips.first().unwrap()).into();
                    }
                    std::cmp::Ordering::Greater => {
                        source_ip = IpAddr::from(
                            source_ips[rand::thread_rng().gen_range(0..source_ips.len())],
                        )
                        .into();
                    }
                    std::cmp::Ordering::Less => (),
                }
            } else {
                let source_ips = self.queue.config.source_ip.ipv6.eval(envelope).await;
                match source_ips.len().cmp(&1) {
                    std::cmp::Ordering::Equal => {
                        source_ip = IpAddr::from(*source_ips.first().unwrap()).into();
                    }
                    std::cmp::Ordering::Greater => {
                        source_ip = IpAddr::from(
                            source_ips[rand::thread_rng().gen_range(0..source_ips.len())],
                        )
                        .into();
                    }
                    std::cmp::Ordering::Less => (),
                }
            }

            Ok((source_ip, remote_ips))
        } else {
            Err(Status::TemporaryFailure(Error::DnsError(format!(
                "No IP addresses found for {:?}.",
                envelope.mx()
            ))))
        }
    }
}

pub(super) trait ToRemoteHost {
    fn to_remote_hosts<'x, 'y: 'x>(
        &'x self,
        domain: &'y str,
        max_mx: usize,
    ) -> Option<Vec<RemoteHost<'_>>>;
}

impl ToRemoteHost for Vec<MX> {
    fn to_remote_hosts<'x, 'y: 'x>(
        &'x self,
        domain: &'y str,
        max_mx: usize,
    ) -> Option<Vec<RemoteHost<'_>>> {
        if !self.is_empty() {
            // Obtain max number of MX hosts to process
            let mut remote_hosts = Vec::with_capacity(max_mx);

            'outer: for mx in self.iter() {
                if mx.exchanges.len() > 1 {
                    let mut slice = mx.exchanges.iter().collect::<Vec<_>>();
                    slice.shuffle(&mut rand::thread_rng());
                    for remote_host in slice {
                        remote_hosts.push(RemoteHost::MX(remote_host.as_str()));
                        if remote_hosts.len() == max_mx {
                            break 'outer;
                        }
                    }
                } else if let Some(remote_host) = mx.exchanges.first() {
                    // Check for Null MX
                    if mx.preference == 0 && remote_host == "." {
                        return None;
                    }
                    remote_hosts.push(RemoteHost::MX(remote_host.as_str()));
                    if remote_hosts.len() == max_mx {
                        break;
                    }
                }
            }
            remote_hosts.into()
        } else {
            // If an empty list of MXs is returned, the address is treated as if it was
            // associated with an implicit MX RR with a preference of 0, pointing to that host.
            vec![RemoteHost::MX(domain)].into()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use mail_auth::{IpLookupStrategy, MX};

    use crate::{config::IfBlock, core::Core, outbound::RemoteHost};

    use super::ToRemoteHost;

    #[tokio::test]
    async fn lookup_ip() {
        let ipv6 = vec![
            "a:b::1".parse().unwrap(),
            "a:b::2".parse().unwrap(),
            "a:b::3".parse().unwrap(),
            "a:b::4".parse().unwrap(),
        ];
        let ipv4 = vec![
            "10.0.0.1".parse().unwrap(),
            "10.0.0.2".parse().unwrap(),
            "10.0.0.3".parse().unwrap(),
            "10.0.0.4".parse().unwrap(),
        ];
        let mut core = Core::test();
        core.queue.config.source_ip.ipv4 = IfBlock::new(ipv4.clone());
        core.queue.config.source_ip.ipv6 = IfBlock::new(ipv6.clone());
        core.resolvers.dns.ipv4_add(
            "mx.foobar.org",
            vec![
                "172.168.0.100".parse().unwrap(),
                "172.168.0.101".parse().unwrap(),
            ],
            Instant::now() + Duration::from_secs(10),
        );
        core.resolvers.dns.ipv6_add(
            "mx.foobar.org",
            vec!["e:f::a".parse().unwrap(), "e:f::b".parse().unwrap()],
            Instant::now() + Duration::from_secs(10),
        );

        // Ipv4 strategy
        core.queue.config.ip_strategy = IfBlock::new(IpLookupStrategy::Ipv4thenIpv6);
        let (source_ips, remote_ips) = core
            .resolve_host(&RemoteHost::MX("mx.foobar.org"), &"envelope", 2)
            .await
            .unwrap();
        assert!(ipv4.contains(&match source_ips.unwrap() {
            std::net::IpAddr::V4(v4) => v4,
            _ => unreachable!(),
        }));
        assert!(remote_ips.contains(&"172.168.0.100".parse().unwrap()));

        // Ipv6 strategy
        core.queue.config.ip_strategy = IfBlock::new(IpLookupStrategy::Ipv6thenIpv4);
        let (source_ips, remote_ips) = core
            .resolve_host(&RemoteHost::MX("mx.foobar.org"), &"envelope", 2)
            .await
            .unwrap();
        assert!(ipv6.contains(&match source_ips.unwrap() {
            std::net::IpAddr::V6(v6) => v6,
            _ => unreachable!(),
        }));
        assert!(remote_ips.contains(&"e:f::a".parse().unwrap()));
    }

    #[test]
    fn to_remote_hosts() {
        let mx = vec![
            MX {
                exchanges: vec!["mx1".to_string(), "mx2".to_string()],
                preference: 10,
            },
            MX {
                exchanges: vec![
                    "mx3".to_string(),
                    "mx4".to_string(),
                    "mx5".to_string(),
                    "mx6".to_string(),
                ],
                preference: 20,
            },
            MX {
                exchanges: vec!["mx7".to_string(), "mx8".to_string()],
                preference: 10,
            },
            MX {
                exchanges: vec!["mx9".to_string(), "mxA".to_string()],
                preference: 10,
            },
        ];
        let hosts = mx.to_remote_hosts("domain", 7).unwrap();
        assert_eq!(hosts.len(), 7);
        for host in hosts {
            if let RemoteHost::MX(host) = host {
                assert!((*host.as_bytes().last().unwrap() - b'0') <= 8);
            }
        }
        let mx = vec![MX {
            exchanges: vec![".".to_string()],
            preference: 0,
        }];
        assert!(mx.to_remote_hosts("domain", 10).is_none());
    }
}
