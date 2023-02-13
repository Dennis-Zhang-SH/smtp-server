use super::{
    utils::{AsKey, ParseKey, ParseValue},
    *,
};

impl Config {
    pub fn parse_throttle(
        &self,
        prefix: impl AsKey,
        ctx: &ConfigContext,
        available_envelope_keys: &[EnvelopeKey],
        available_throttle_keys: u16,
    ) -> super::Result<Vec<Throttle>> {
        let prefix_ = prefix.as_key();
        let mut throttles = Vec::new();
        for array_pos in self.sub_keys(prefix) {
            throttles.push(self.parse_throttle_item(
                (&prefix_, array_pos),
                ctx,
                available_envelope_keys,
                available_throttle_keys,
            )?);
        }

        Ok(throttles)
    }

    fn parse_throttle_item(
        &self,
        prefix: impl AsKey,
        ctx: &ConfigContext,
        available_envelope_keys: &[EnvelopeKey],
        available_throttle_keys: u16,
    ) -> super::Result<Throttle> {
        let prefix = prefix.as_key();
        let mut keys = 0;
        for (key_, value) in self.values((&prefix, "key")) {
            let key = value.parse_throttle_key(key_)?;
            if (key & available_throttle_keys) != 0 {
                keys |= key;
            } else {
                return Err(format!(
                    "Throttle key {value:?} is not available in this context for property {key_:?}"
                ));
            }
        }

        let throttle = Throttle {
            conditions: if self.values((&prefix, "match")).next().is_some() {
                self.parse_condition((&prefix, "match"), ctx, available_envelope_keys)?
            } else {
                Conditions {
                    conditions: Vec::with_capacity(0),
                }
            },
            keys,
            concurrency: self
                .property::<u64>((prefix.as_str(), "concurrency"))?
                .filter(|&v| v > 0),
            rate: self
                .property::<Rate>((prefix.as_str(), "rate"))?
                .filter(|v| v.requests > 0),
        };

        // Validate
        if throttle.rate.is_none() && throttle.concurrency.is_none() {
            Err(format!(
                concat!(
                    "Throttle {:?} needs to define a ",
                    "valid 'rate' and/or 'concurrency' property."
                ),
                prefix
            ))
        } else {
            Ok(throttle)
        }
    }
}

impl ParseValue for Rate {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        if let Some((requests, period)) = value.split_once('/') {
            Ok(Rate {
                requests: requests
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .and_then(|r| if r > 0 { Some(r) } else { None })
                    .ok_or_else(|| {
                        format!(
                            "Invalid rate value {:?} for property {:?}.",
                            value,
                            key.as_key()
                        )
                    })?,
                period: period.parse_key(key)?,
            })
        } else if ["false", "none", "unlimited"].contains(&value) {
            Ok(Rate::default())
        } else {
            Err(format!(
                "Invalid rate value {:?} for property {:?}.",
                value,
                key.as_key()
            ))
        }
    }
}

impl ParseValue for EnvelopeKey {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        Ok(match value {
            "rcpt" => EnvelopeKey::Recipient,
            "rcpt-domain" => EnvelopeKey::RecipientDomain,
            "sender" => EnvelopeKey::Sender,
            "sender-domain" => EnvelopeKey::SenderDomain,
            "listener" => EnvelopeKey::Listener,
            "remote-ip" => EnvelopeKey::RemoteIp,
            "local-ip" => EnvelopeKey::LocalIp,
            "priority" => EnvelopeKey::Priority,
            "authenticated-as" => EnvelopeKey::AuthenticatedAs,
            "mx" => EnvelopeKey::Mx,
            _ => {
                return Err(format!(
                    "Invalid context key {:?} for property {:?}.",
                    value,
                    key.as_key()
                ))
            }
        })
    }
}

pub trait ParseTrottleKey {
    fn parse_throttle_key(&self, key: &str) -> super::Result<u16>;
}

impl ParseTrottleKey for &str {
    fn parse_throttle_key(&self, key: &str) -> super::Result<u16> {
        match *self {
            "rcpt" => Ok(THROTTLE_RCPT),
            "rcpt-domain" => Ok(THROTTLE_RCPT_DOMAIN),
            "sender" => Ok(THROTTLE_SENDER),
            "sender-domain" => Ok(THROTTLE_SENDER_DOMAIN),
            "authenticated-as" => Ok(THROTTLE_AUTH_AS),
            "listener" => Ok(THROTTLE_LISTENER),
            "mx" => Ok(THROTTLE_MX),
            "remote-ip" => Ok(THROTTLE_REMOTE_IP),
            "local-ip" => Ok(THROTTLE_LOCAL_IP),
            "helo-domain" => Ok(THROTTLE_HELO_DOMAIN),
            _ => Err(format!("Invalid throttle key {self:?} found in {key:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use crate::config::{
        Condition, ConditionMatch, Conditions, Config, ConfigContext, EnvelopeKey, IpAddrMask,
        Rate, Throttle, THROTTLE_AUTH_AS, THROTTLE_REMOTE_IP, THROTTLE_SENDER_DOMAIN,
    };

    #[test]
    fn parse_throttle() {
        let mut file = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        file.push("resources");
        file.push("tests");
        file.push("config");
        file.push("throttle.toml");

        let available_keys = vec![
            EnvelopeKey::Recipient,
            EnvelopeKey::RecipientDomain,
            EnvelopeKey::Sender,
            EnvelopeKey::SenderDomain,
            EnvelopeKey::AuthenticatedAs,
            EnvelopeKey::Listener,
            EnvelopeKey::RemoteIp,
            EnvelopeKey::LocalIp,
            EnvelopeKey::Priority,
        ];

        let config = Config::parse(&fs::read_to_string(file).unwrap()).unwrap();
        let context = ConfigContext::default();
        let throttle = config
            .parse_throttle("throttle", &context, &available_keys, u16::MAX)
            .unwrap();

        assert_eq!(
            throttle,
            vec![
                Throttle {
                    conditions: Conditions {
                        conditions: vec![Condition::Match {
                            key: EnvelopeKey::RemoteIp,
                            value: ConditionMatch::IpAddrMask(IpAddrMask::V4 {
                                addr: "127.0.0.1".parse().unwrap(),
                                mask: u32::MAX
                            }),
                            not: false
                        }]
                    },
                    keys: THROTTLE_REMOTE_IP | THROTTLE_AUTH_AS,
                    concurrency: 100.into(),
                    rate: Rate {
                        requests: 50,
                        period: Duration::from_secs(30)
                    }
                    .into()
                },
                Throttle {
                    conditions: Conditions { conditions: vec![] },
                    keys: THROTTLE_SENDER_DOMAIN,
                    concurrency: 10000.into(),
                    rate: None
                }
            ]
        );
    }
}
