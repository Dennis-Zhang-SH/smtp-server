use std::time::Duration;

use sieve::{compiler::grammar::Capability, Compiler, Runtime};

use crate::core::{SieveConfig, SieveCore};

use super::{utils::AsKey, Config, ConfigContext};

impl Config {
    pub fn parse_sieve(&self, ctx: &mut ConfigContext) -> super::Result<SieveCore> {
        // Allocate compiler and runtime
        let compiler = Compiler::new()
            .with_max_string_size(52428800)
            .with_max_string_size(10240)
            .with_max_variable_name_size(100)
            .with_max_nested_blocks(50)
            .with_max_nested_tests(50)
            .with_max_nested_foreverypart(10)
            .with_max_local_variables(128)
            .with_max_header_size(10240)
            .with_max_includes(10);
        let mut runtime = Runtime::new()
            .without_capabilities([
                Capability::FileInto,
                Capability::Vacation,
                Capability::VacationSeconds,
                Capability::Fcc,
                Capability::Mailbox,
                Capability::MailboxId,
                Capability::MboxMetadata,
                Capability::ServerMetadata,
                Capability::ImapSieve,
                Capability::Duplicate,
            ])
            .with_max_variable_size(102400)
            .with_max_header_size(10240)
            .with_valid_notification_uri("mailto")
            .with_valid_ext_lists(ctx.lookup.keys().map(|k| k.to_string()));

        if let Some(value) = self.property("sieve.limits.redirects")? {
            runtime.set_max_redirects(value);
        }
        if let Some(value) = self.property("sieve.limits.out-messages")? {
            runtime.set_max_out_messages(value);
        }
        if let Some(value) = self.property("sieve.limits.cpu")? {
            runtime.set_cpu_limit(value);
        }
        if let Some(value) = self.property("sieve.limits.nested-includes")? {
            runtime.set_max_nested_includes(value);
        }
        if let Some(value) = self.property("sieve.limits.received-headers")? {
            runtime.set_max_received_headers(value);
        }
        if let Some(value) = self.property::<Duration>("sieve.limits.duplicate-expiry")? {
            runtime.set_default_duplicate_expiry(value.as_secs());
        }
        let hostname = if let Some(hostname) = self.value("sieve.hostname") {
            hostname
        } else {
            self.value_require("server.hostname")?
        };
        runtime.set_local_hostname(hostname.to_string());

        // Parse scripts
        for id in self.sub_keys("sieve.scripts") {
            let script = self.file_contents(("sieve.scripts", id))?;
            ctx.scripts.insert(
                id.to_string(),
                compiler
                    .compile(&script)
                    .unwrap_or_else(|err| panic!("Failed to compile Sieve script {id:?}: {err}"))
                    .into(),
            );
        }

        // Parse DKIM signatures
        let mut sign = Vec::new();
        for (pos, id) in self.values("sieve.sign") {
            if let Some(dkim) = ctx.signers.get(id) {
                sign.push(dkim.clone());
            } else {
                return Err(format!(
                    "No DKIM signer found with id {:?} for key {:?}.",
                    id,
                    ("sieve.sign", pos).as_key()
                ));
            }
        }

        Ok(SieveCore {
            runtime,
            scripts: ctx.scripts.clone(),
            lookup: ctx.lookup.clone(),
            config: SieveConfig {
                from_addr: self
                    .value("sieve.from-addr")
                    .map(|a| a.to_string())
                    .unwrap_or(format!("MAILER-DAEMON@{hostname}")),
                from_name: self
                    .value("sieve.from-name")
                    .unwrap_or("Mailer Daemon")
                    .to_string(),
                return_path: self
                    .value("sieve.return-path")
                    .unwrap_or_default()
                    .to_string(),
                sign,
                db: if let Some(db) = self.value("sieve.use-database") {
                    if let Some(db) = ctx.databases.get(db) {
                        Some(db.clone())
                    } else {
                        return Err(format!(
                            "Database {db:?} not found for key \"sieve.use-database\"."
                        ));
                    }
                } else {
                    None
                },
            },
        })
    }
}
