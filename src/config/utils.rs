use std::{net::IpAddr, time::Duration};

use super::Config;

impl Config {
    pub fn property<T: ParseValue>(&self, key: impl AsKey) -> super::Result<Option<T>> {
        let key = key.as_key();
        if let Some(value) = self.keys.get(&key) {
            T::parse_value(key, value).map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn property_or_default<T: ParseValue>(
        &self,
        key: impl AsKey,
        default: impl AsKey,
    ) -> super::Result<Option<T>> {
        match self.property(key) {
            Ok(None) => self.property(default),
            result => result,
        }
    }

    pub fn property_require<T: ParseValue>(&self, key: impl AsKey) -> super::Result<T> {
        match self.property(key.clone()) {
            Ok(Some(result)) => Ok(result),
            Ok(None) => Err(format!("Missing property {:?}.", key.as_key())),
            Err(err) => Err(err),
        }
    }

    pub fn sub_keys<'x, 'y: 'x>(&'y self, prefix: impl AsKey) -> impl Iterator<Item = &str> + 'x {
        let mut last_key = "";
        let prefix = prefix.as_prefix();

        self.keys.keys().filter_map(move |key| {
            let key = key.strip_prefix(&prefix)?;
            let key = if let Some((key, _)) = key.split_once('.') {
                key
            } else {
                key
            };
            if last_key != key {
                last_key = key;
                Some(key)
            } else {
                None
            }
        })
    }

    pub fn properties<T: ParseValue>(
        &self,
        prefix: impl AsKey,
    ) -> impl Iterator<Item = super::Result<(&str, T)>> {
        let full_prefix = prefix.as_key();
        let prefix = prefix.as_prefix();

        self.keys.iter().filter_map(move |(key, value)| {
            if key.starts_with(&prefix) || key == &full_prefix {
                T::parse_value(key.as_str(), value)
                    .map(|value| (key.as_str(), value))
                    .into()
            } else {
                None
            }
        })
    }

    pub fn value(&self, key: impl AsKey) -> Option<&str> {
        self.keys.get(&key.as_key()).map(|s| s.as_str())
    }

    pub fn value_require(&self, key: impl AsKey) -> super::Result<&str> {
        self.keys
            .get(&key.as_key())
            .map(|s| s.as_str())
            .ok_or_else(|| format!("Missing property {:?}.", key.as_key()))
    }

    pub fn value_or_default(&self, key: impl AsKey, default: impl AsKey) -> Option<&str> {
        self.keys
            .get(&key.as_key())
            .or_else(|| self.keys.get(&default.as_key()))
            .map(|s| s.as_str())
    }

    pub fn values(&self, prefix: impl AsKey) -> impl Iterator<Item = (&str, &str)> {
        let full_prefix = prefix.as_key();
        let prefix = prefix.as_prefix();

        self.keys.iter().filter_map(move |(key, value)| {
            if key.starts_with(&prefix) || key == &full_prefix {
                (key.as_str(), value.as_str()).into()
            } else {
                None
            }
        })
    }

    pub fn values_or_default(
        &self,
        prefix: impl AsKey,
        default: impl AsKey,
    ) -> impl Iterator<Item = (&str, &str)> {
        let mut prefix = prefix.as_prefix();

        self.values(if self.keys.keys().any(|k| k.starts_with(&prefix)) {
            prefix.truncate(prefix.len() - 1);
            prefix
        } else {
            default.as_key()
        })
    }

    pub fn take_value(&mut self, key: &str) -> Option<String> {
        self.keys.remove(key)
    }

    pub fn file_contents(&self, key: impl AsKey) -> super::Result<Vec<u8>> {
        let key = key.as_key();
        if let Some(value) = self.keys.get(&key) {
            if value.starts_with("file://") {
                std::fs::read(value).map_err(|err| {
                    format!(
                        "Failed to read file {:?} for property {:?}: {}",
                        value, key, err
                    )
                })
            } else {
                Ok(value.to_string().into_bytes())
            }
        } else {
            Err(format!(
                "Property {:?} not found in configuration file.",
                key
            ))
        }
    }

    pub fn parse_values<T: ParseValues>(&self, prefix: impl AsKey) -> super::Result<T> {
        let mut result = T::default();
        for (pos, (key, value)) in self.values(prefix.clone()).enumerate() {
            if pos == 0 || T::is_multivalue() {
                result.add_value(T::Item::parse_value(key, value)?);
            } else {
                return Err(format!(
                    "Property {:?} cannot have multiple values.",
                    prefix.as_key()
                ));
            }
        }
        Ok(result)
    }
}

pub trait ParseValues: Sized + Default {
    type Item: ParseValue;

    fn add_value(&mut self, value: Self::Item);
    fn is_multivalue() -> bool;
}

pub trait ParseValue: Sized {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self>;
}

pub trait ParseKey<T: ParseValue> {
    fn parse_key(&self, key: impl AsKey) -> super::Result<T>;
}

impl<T: ParseValue> ParseKey<T> for &str {
    fn parse_key(&self, key: impl AsKey) -> super::Result<T> {
        T::parse_value(key, self)
    }
}

impl<T: ParseValue> ParseKey<T> for String {
    fn parse_key(&self, key: impl AsKey) -> super::Result<T> {
        T::parse_value(key, self.as_str())
    }
}

impl<T: ParseValue> ParseKey<T> for &String {
    fn parse_key(&self, key: impl AsKey) -> super::Result<T> {
        T::parse_value(key, self.as_str())
    }
}

impl<T: ParseValue> ParseValues for Vec<T> {
    type Item = T;

    fn add_value(&mut self, value: Self::Item) {
        self.push(value);
    }

    fn is_multivalue() -> bool {
        true
    }
}

impl<T: ParseValue + Default> ParseValues for T {
    type Item = T;

    fn add_value(&mut self, value: Self::Item) {
        *self = value;
    }

    fn is_multivalue() -> bool {
        false
    }
}

impl ParseValue for String {
    fn parse_value(_key: impl AsKey, value: &str) -> super::Result<Self> {
        Ok(value.to_string())
    }
}

impl ParseValue for u64 {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid integer value {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for i64 {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid integer value {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for u32 {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid integer value {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for IpAddr {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid IP address value {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for usize {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid integer value {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for bool {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid boolean value {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for Duration {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        let duration = value.trim().to_ascii_uppercase();
        let (num, multiplier) = if let Some(num) = duration.strip_prefix('d') {
            (num, 24 * 60 * 60 * 1000)
        } else if let Some(num) = duration.strip_prefix('h') {
            (num, 60 * 60 * 1000)
        } else if let Some(num) = duration.strip_prefix('m') {
            (num, 60 * 1000)
        } else if let Some(num) = duration.strip_prefix('s') {
            (num, 1000)
        } else if let Some(num) = duration.strip_prefix("ms") {
            (num, 1)
        } else {
            (duration.as_str(), 1)
        };
        num.parse::<u64>()
            .ok()
            .and_then(|num| {
                if num > 0 {
                    Some(Duration::from_millis(num * multiplier))
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                format!(
                    "Invalid duration value {:?} for property {:?}.",
                    value,
                    key.as_key()
                )
            })
    }
}

pub trait AsKey: Clone {
    fn as_key(&self) -> String;
    fn as_prefix(&self) -> String;
}

impl AsKey for &str {
    fn as_key(&self) -> String {
        self.to_string()
    }

    fn as_prefix(&self) -> String {
        format!("{}.", self)
    }
}

impl AsKey for String {
    fn as_key(&self) -> String {
        self.to_string()
    }

    fn as_prefix(&self) -> String {
        format!("{}.", self)
    }
}

impl AsKey for (&str, &str) {
    fn as_key(&self) -> String {
        format!("{}.{}", self.0, self.1)
    }

    fn as_prefix(&self) -> String {
        format!("{}.{}.", self.0, self.1)
    }
}

impl AsKey for (&str, &str, &str) {
    fn as_key(&self) -> String {
        format!("{}.{}.{}", self.0, self.1, self.2)
    }

    fn as_prefix(&self) -> String {
        format!("{}.{}.{}.", self.0, self.1, self.2)
    }
}

impl AsKey for (&str, &str, &str, &str) {
    fn as_key(&self) -> String {
        format!("{}.{}.{}.{}", self.0, self.1, self.2, self.3)
    }

    fn as_prefix(&self) -> String {
        format!("{}.{}.{}.{}.", self.0, self.1, self.2, self.3)
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use crate::config::Config;

    #[test]
    fn toml_utils() {
        let toml = r#"
[queues."z"]
retry = [0, 1, 15, 60, 90]
value = "hi"

[queues."x"]
retry = [3, 60]
value = "hi 2"

[queues.a]
retry = [1, 2, 3, 4]
value = "hi 3"

[servers."my relay"]
hostname = "mx.example.org"

[[servers."my relay".transaction.auth.limits]]
idle = 10

[[servers."my relay".transaction.auth.limits]]
idle = 20

[servers."submissions"]
hostname = "submit.example.org"
ip = a:b::1:1
"#;
        let config = Config::parse(toml).unwrap();

        assert_eq!(
            config.sub_keys("queues").collect::<Vec<_>>(),
            ["a", "x", "z"]
        );
        assert_eq!(
            config.sub_keys("servers").collect::<Vec<_>>(),
            ["my relay", "submissions"]
        );
        assert_eq!(
            config.sub_keys("queues.z.retry").collect::<Vec<_>>(),
            ["0", "1", "2", "3", "4"]
        );
        assert_eq!(
            config
                .property::<u32>("servers.my relay.transaction.auth.limits.1.idle")
                .unwrap()
                .unwrap(),
            20
        );
        assert_eq!(
            config
                .property::<IpAddr>(("servers", "submissions", "ip"))
                .unwrap()
                .unwrap(),
            "a:b::1:1".parse::<IpAddr>().unwrap()
        );
    }
}
