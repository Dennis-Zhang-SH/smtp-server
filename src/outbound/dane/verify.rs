use mail_auth::{
    sha1::Digest,
    sha2::{Sha256, Sha512},
};
use rustls::Certificate;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::queue::{Error, ErrorDetails, Status};

use super::Tlsa;

impl Tlsa {
    pub fn verify(
        &self,
        span: &tracing::Span,
        hostname: &str,
        certificates: Option<&[Certificate]>,
    ) -> Result<(), Status<(), Error>> {
        let certificates = if let Some(certificates) = certificates {
            certificates
        } else {
            tracing::info!(
                parent: span,
                context = "dane",
                event = "no-server-certs-found",
                mx = hostname,
                "No certificates were provided."
            );
            return Err(Status::TemporaryFailure(Error::DaneError(ErrorDetails {
                entity: hostname.to_string(),
                details: "No certificates were provided by host".to_string(),
            })));
        };

        let mut matched_end_entity = false;
        let mut matched_intermediate = false;
        'outer: for (pos, der_certificate) in certificates.iter().enumerate() {
            // Parse certificate
            let certificate = match X509Certificate::from_der(der_certificate.as_ref()) {
                Ok((_, certificate)) => certificate,
                Err(err) => {
                    tracing::debug!(
                        parent: span,
                        context = "dane",
                        event = "cert-parse-error",
                        "Failed to parse X.509 certificate for host {}: {}",
                        hostname,
                        err
                    );
                    return Err(Status::TemporaryFailure(Error::DaneError(ErrorDetails {
                        entity: hostname.to_string(),
                        details: "Failed to parse X.509 certificate".to_string(),
                    })));
                }
            };

            // Match against TLSA records
            let is_end_entity = pos == 0;
            let mut sha256 = [None, None];
            let mut sha512 = [None, None];
            for record in self.entries.iter() {
                if record.is_end_entity == is_end_entity {
                    let hash: &[u8] = if record.is_sha256 {
                        &sha256[usize::from(record.is_spki)].get_or_insert_with(|| {
                            let mut hasher = Sha256::new();
                            hasher.update(if record.is_spki {
                                certificate.public_key().raw
                            } else {
                                der_certificate.as_ref()
                            });
                            hasher.finalize()
                        })[..]
                    } else {
                        &sha512[usize::from(record.is_spki)].get_or_insert_with(|| {
                            let mut hasher = Sha512::new();
                            hasher.update(if record.is_spki {
                                certificate.public_key().raw
                            } else {
                                der_certificate.as_ref()
                            });
                            hasher.finalize()
                        })[..]
                    };

                    if hash == record.data {
                        tracing::debug!(
                            parent: span,
                            context = "dane",
                            event = "info",
                            mx = hostname,
                            certificate = if is_end_entity {
                                "end-entity"
                            } else {
                                "intermediate"
                            },
                            "Matched TLSA record with hash {:x?}.",
                            hash
                        );

                        if is_end_entity {
                            matched_end_entity = true;
                            if !self.has_intermediates {
                                break 'outer;
                            }
                        } else {
                            matched_intermediate = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        if (self.has_end_entities == matched_end_entity)
            && (self.has_intermediates == matched_intermediate)
        {
            tracing::info!(
                parent: span,
                context = "dane",
                event = "authenticated",
                mx = hostname,
                "DANE authentication successful.",
            );
            Ok(())
        } else {
            tracing::warn!(
                parent: span,
                context = "dane",
                event = "auth-failure",
                mx = hostname,
                "No matching certificates found in TLSA records.",
            );
            Err(Status::PermanentFailure(Error::DaneError(ErrorDetails {
                entity: hostname.to_string(),
                details: "No matching certificates found in TLSA records".to_string(),
            })))
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::BTreeSet,
        fs::{self, File},
        io::{BufRead, BufReader},
        num::ParseIntError,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use mail_auth::{
        common::lru::{DnsCache, LruCache},
        trust_dns_resolver::{
            config::{ResolverConfig, ResolverOpts},
            AsyncResolver,
        },
        Resolver,
    };
    use rustls::Certificate;

    use crate::{
        core::Resolvers,
        outbound::dane::{DnssecResolver, Tlsa, TlsaEntry},
        queue::{Error, ErrorDetails, Status},
    };

    #[tokio::test]
    async fn dane_test() {
        let conf = ResolverConfig::cloudflare_tls();
        let mut opts = ResolverOpts::default();
        opts.validate = true;
        opts.try_tcp_on_error = true;

        let r = Resolvers {
            dns: Resolver::new_cloudflare().unwrap(),
            dnssec: DnssecResolver {
                resolver: AsyncResolver::tokio(conf, opts).unwrap(),
            },
            cache: crate::core::DnsCache {
                tlsa: LruCache::with_capacity(10),
                mta_sts: LruCache::with_capacity(10),
            },
        };

        // Add dns entries
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("resources");
        path.push("tests");
        path.push("dane");
        let mut file = path.clone();
        file.push("dns.txt");

        let mut hosts = BTreeSet::new();
        let mut tlsa = Tlsa {
            entries: Vec::new(),
            has_end_entities: false,
            has_intermediates: false,
        };
        let mut hostname = String::new();

        for line in BufReader::new(File::open(file).unwrap()).lines() {
            let line = line.unwrap();
            let mut is_end_entity = false;
            for (pos, item) in line.split_whitespace().enumerate() {
                match pos {
                    0 => {
                        if hostname != item && !hostname.is_empty() {
                            r.tlsa_add(hostname, tlsa, Instant::now() + Duration::from_secs(30));
                            tlsa = Tlsa {
                                entries: Vec::new(),
                                has_end_entities: false,
                                has_intermediates: false,
                            };
                        }
                        hosts.insert(item.strip_prefix("_25._tcp.").unwrap().to_string());
                        hostname = item.to_string();
                    }
                    1 => {
                        is_end_entity = item == "3";
                    }
                    4 => {
                        if is_end_entity {
                            tlsa.has_end_entities = true;
                        } else {
                            tlsa.has_intermediates = true;
                        }
                        tlsa.entries.push(TlsaEntry {
                            is_end_entity,
                            is_sha256: true,
                            is_spki: true,
                            data: decode_hex(item).unwrap(),
                        });
                    }
                    _ => (),
                }
                if pos == 0 {}
            }
        }
        r.tlsa_add(hostname, tlsa, Instant::now() + Duration::from_secs(30));

        // Add certificates
        assert!(!hosts.is_empty());
        for host in hosts {
            // Add certificates
            let mut certs = Vec::new();
            for num in 0..6 {
                let mut file = path.clone();
                file.push(format!("{}.{}.cert", host, num));
                if file.exists() {
                    certs.push(Certificate(fs::read(file).unwrap()));
                } else {
                    break;
                }
            }

            // Successful DANE verification
            let tlsa = r.tlsa_lookup(&host).await.unwrap().unwrap();

            assert_eq!(
                tlsa.verify(&tracing::info_span!("test_span"), &host, Some(&certs)),
                Ok(())
            );

            // Failed DANE verification
            certs.remove(0);
            assert_eq!(
                tlsa.verify(&tracing::info_span!("test_span"), &host, Some(&certs)),
                Err(Status::PermanentFailure(Error::DaneError(ErrorDetails {
                    entity: host.to_string(),
                    details: "No matching certificates found in TLSA records".to_string()
                })))
            );
        }
    }

    pub fn decode_hex(s: &str) -> Result<Vec<u8>, ParseIntError> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect()
    }
}
