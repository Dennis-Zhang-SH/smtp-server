use std::{
    borrow::Cow,
    collections::hash_map::Entry,
    io::{Cursor, Read},
    sync::{atomic::Ordering, Arc},
    time::SystemTime,
};

use ahash::AHashMap;
use mail_auth::{
    flate2::read::GzDecoder,
    report::{tlsrpt::TlsReport, ActionDisposition, DmarcResult, Feedback, Report},
    zip,
};
use mail_parser::{DateTime, HeaderValue, Message, MimeHeaders, PartType};

use crate::core::Core;

enum Compression {
    None,
    Gzip,
    Zip,
}

enum Format {
    Dmarc,
    Tls,
    Arf,
}

struct ReportData<'x> {
    compression: Compression,
    format: Format,
    data: &'x [u8],
}

pub trait AnalyzeReport {
    fn analyze_report(&self, message: Arc<Vec<u8>>);
}

impl AnalyzeReport for Arc<Core> {
    fn analyze_report(&self, message: Arc<Vec<u8>>) {
        let core = self.clone();
        self.worker_pool.spawn(move || {
            let message = if let Some(message) = Message::parse(&message) {
                message
            } else {
                tracing::debug!(context = "report", "Failed to parse message.");
                return;
            };
            let from = match message.from() {
                HeaderValue::Address(addr) => addr.address.as_ref().map(|a| a.as_ref()),
                HeaderValue::AddressList(addr_list) => addr_list
                    .last()
                    .and_then(|a| a.address.as_ref())
                    .map(|a| a.as_ref()),
                _ => None,
            }
            .unwrap_or("unknown");
            let mut reports = Vec::new();

            for part in &message.parts {
                match &part.body {
                    PartType::Text(report) => {
                        if part
                            .content_type()
                            .and_then(|ct| ct.subtype())
                            .map_or(false, |t| t.eq_ignore_ascii_case("xml"))
                            || part
                                .attachment_name()
                                .and_then(|n| n.rsplit_once('.'))
                                .map_or(false, |(_, e)| e.eq_ignore_ascii_case("xml"))
                        {
                            reports.push(ReportData {
                                compression: Compression::None,
                                format: Format::Dmarc,
                                data: report.as_bytes(),
                            });
                        } else if part.is_content_type("message", "feedback-report") {
                            reports.push(ReportData {
                                compression: Compression::None,
                                format: Format::Arf,
                                data: report.as_bytes(),
                            });
                        }
                    }
                    PartType::Binary(report) | PartType::InlineBinary(report) => {
                        if part.is_content_type("message", "feedback-report") {
                            reports.push(ReportData {
                                compression: Compression::None,
                                format: Format::Arf,
                                data: report.as_ref(),
                            });
                            continue;
                        }

                        let subtype = part
                            .content_type()
                            .and_then(|ct| ct.subtype())
                            .unwrap_or("");
                        let attachment_name = part.attachment_name();
                        let ext = attachment_name
                            .and_then(|f| f.rsplit_once('.'))
                            .map_or("", |(_, e)| e);
                        let tls_parts = subtype.rsplit_once('+');
                        let compression = match (tls_parts.map(|(_, c)| c).unwrap_or(subtype), ext)
                        {
                            ("gzip", _) => Compression::Gzip,
                            ("zip", _) => Compression::Zip,
                            (_, "gz") => Compression::Gzip,
                            (_, "zip") => Compression::Zip,
                            _ => Compression::None,
                        };
                        let format = match (tls_parts.map(|(c, _)| c).unwrap_or(subtype), ext) {
                            ("xml", _) => Format::Dmarc,
                            ("tlsrpt", _) | (_, "json") => Format::Tls,
                            _ => {
                                if attachment_name
                                    .map_or(false, |n| n.contains(".xml") || n.contains('!'))
                                {
                                    Format::Dmarc
                                } else {
                                    continue;
                                }
                            }
                        };

                        reports.push(ReportData {
                            compression,
                            format,
                            data: report.as_ref(),
                        });
                    }
                    _ => (),
                }
            }

            for report in reports {
                let data = match report.compression {
                    Compression::None => Cow::Borrowed(report.data),
                    Compression::Gzip => {
                        let mut file = GzDecoder::new(report.data);
                        let mut buf = Vec::new();
                        if let Err(err) = file.read_to_end(&mut buf) {
                            tracing::debug!(
                                context = "report",
                                from = from,
                                "Failed to decompress report: {}",
                                err
                            );
                            continue;
                        }
                        Cow::Owned(buf)
                    }
                    Compression::Zip => {
                        let mut archive = match zip::ZipArchive::new(Cursor::new(report.data)) {
                            Ok(archive) => archive,
                            Err(err) => {
                                tracing::debug!(
                                    context = "report",
                                    from = from,
                                    "Failed to decompress report: {}",
                                    err
                                );
                                continue;
                            }
                        };
                        let mut buf = Vec::with_capacity(0);
                        for i in 0..archive.len() {
                            match archive.by_index(i) {
                                Ok(mut file) => {
                                    buf = Vec::with_capacity(file.compressed_size() as usize);
                                    if let Err(err) = file.read_to_end(&mut buf) {
                                        tracing::debug!(
                                            context = "report",
                                            from = from,
                                            "Failed to decompress report: {}",
                                            err
                                        );
                                    }
                                    break;
                                }
                                Err(err) => {
                                    tracing::debug!(
                                        context = "report",
                                        from = from,
                                        "Failed to decompress report: {}",
                                        err
                                    );
                                }
                            }
                        }
                        Cow::Owned(buf)
                    }
                };

                match report.format {
                    Format::Dmarc => match Report::parse_xml(&data) {
                        Ok(report) => {
                            report.log();
                        }
                        Err(err) => {
                            tracing::debug!(
                                context = "report",
                                from = from,
                                "Failed to parse DMARC report: {}",
                                err
                            );
                            continue;
                        }
                    },
                    Format::Tls => match TlsReport::parse_json(&data) {
                        Ok(report) => {
                            report.log();
                        }
                        Err(err) => {
                            tracing::debug!(
                                context = "report",
                                from = from,
                                "Failed to parse TLS report: {:?}",
                                err
                            );
                            continue;
                        }
                    },
                    Format::Arf => match Feedback::parse_arf(&data) {
                        Some(report) => {
                            report.log();
                        }
                        None => {
                            tracing::debug!(
                                context = "report",
                                from = from,
                                "Failed to parse Auth Failure report"
                            );
                            continue;
                        }
                    },
                }

                // Save report
                if let Some(report_path) = &core.report.config.analysis.store {
                    let (report_format, extension) = match report.format {
                        Format::Dmarc => ("dmarc", "xml"),
                        Format::Tls => ("tlsrpt", "json"),
                        Format::Arf => ("arf", "txt"),
                    };
                    let c_extension = match report.compression {
                        Compression::None => "",
                        Compression::Gzip => ".gz",
                        Compression::Zip => ".zip",
                    };
                    let now = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let id = core
                        .report
                        .config
                        .analysis
                        .report_id
                        .fetch_add(1, Ordering::Relaxed);

                    // Build path
                    let mut report_path = report_path.clone();
                    report_path.push(format!(
                        "{}_{}_{}.{}{}",
                        report_format, now, id, extension, c_extension
                    ));
                    if let Err(err) = std::fs::write(&report_path, report.data) {
                        tracing::warn!(
                            context = "report",
                            event = "error",
                            from = from,
                            "Failed to write incoming report to {}: {}",
                            report_path.display(),
                            err
                        );
                    }
                }
                break;
            }
        });
    }
}

trait LogReport {
    fn log(&self);
}

impl LogReport for Report {
    fn log(&self) {
        let mut dmarc_pass = 0;
        let mut dmarc_quarantine = 0;
        let mut dmarc_reject = 0;
        let mut dmarc_none = 0;
        let mut dkim_pass = 0;
        let mut dkim_fail = 0;
        let mut dkim_none = 0;
        let mut spf_pass = 0;
        let mut spf_fail = 0;
        let mut spf_none = 0;

        for record in self.records() {
            let count = std::cmp::min(record.count(), 1);

            match record.action_disposition() {
                ActionDisposition::Pass => {
                    dmarc_pass += count;
                }
                ActionDisposition::Quarantine => {
                    dmarc_quarantine += count;
                }
                ActionDisposition::Reject => {
                    dmarc_reject += count;
                }
                ActionDisposition::None | ActionDisposition::Unspecified => {
                    dmarc_none += count;
                }
            }
            match record.dmarc_dkim_result() {
                DmarcResult::Pass => {
                    dkim_pass += count;
                }
                DmarcResult::Fail => {
                    dkim_fail += count;
                }
                DmarcResult::Unspecified => {
                    dkim_none += count;
                }
            }
            match record.dmarc_spf_result() {
                DmarcResult::Pass => {
                    spf_pass += count;
                }
                DmarcResult::Fail => {
                    spf_fail += count;
                }
                DmarcResult::Unspecified => {
                    spf_none += count;
                }
            }
        }

        let range_from = DateTime::from_timestamp(self.date_range_begin() as i64).to_rfc3339();
        let range_to = DateTime::from_timestamp(self.date_range_end() as i64).to_rfc3339();

        if (dmarc_reject + dmarc_quarantine + dkim_fail + spf_fail) > 0 {
            tracing::warn!(
                context = "dmarc",
                event = "analyze",
                range_from = range_from,
                range_to = range_to,
                domain = self.domain(),
                report_email = self.email(),
                report_id = self.report_id(),
                dmarc_pass = dmarc_pass,
                dmarc_quarantine = dmarc_quarantine,
                dmarc_reject = dmarc_reject,
                dmarc_none = dmarc_none,
                dkim_pass = dkim_pass,
                dkim_fail = dkim_fail,
                dkim_none = dkim_none,
                spf_pass = spf_pass,
                spf_fail = spf_fail,
                spf_none = spf_none,
            );
        } else {
            tracing::info!(
                context = "dmarc",
                event = "analyze",
                range_from = range_from,
                range_to = range_to,
                domain = self.domain(),
                report_email = self.email(),
                report_id = self.report_id(),
                dmarc_pass = dmarc_pass,
                dmarc_quarantine = dmarc_quarantine,
                dmarc_reject = dmarc_reject,
                dmarc_none = dmarc_none,
                dkim_pass = dkim_pass,
                dkim_fail = dkim_fail,
                dkim_none = dkim_none,
                spf_pass = spf_pass,
                spf_fail = spf_fail,
                spf_none = spf_none,
            );
        }
    }
}

impl LogReport for TlsReport {
    fn log(&self) {
        for policy in self.policies.iter().take(5) {
            let mut details = AHashMap::with_capacity(policy.failure_details.len());
            for failure in &policy.failure_details {
                let num_failures = std::cmp::min(1, failure.failed_session_count);
                match details.entry(failure.result_type) {
                    Entry::Occupied(mut e) => {
                        *e.get_mut() += num_failures;
                    }
                    Entry::Vacant(e) => {
                        e.insert(num_failures);
                    }
                }
            }

            if policy.summary.total_failure > 0 {
                tracing::warn!(
                    context = "tlsrpt",
                    event = "analyze",
                    range_from = self.date_range.start_datetime.to_rfc3339(),
                    range_to = self.date_range.end_datetime.to_rfc3339(),
                    domain = policy.policy.policy_domain,
                    report_contact = self.contact_info.as_deref().unwrap_or("unknown"),
                    report_id = self.report_id,
                    policy_type = ?policy.policy.policy_type,
                    total_success = policy.summary.total_success,
                    total_failures = policy.summary.total_failure,
                    details = ?details,
                );
            } else {
                tracing::info!(
                    context = "tlsrpt",
                    event = "analyze",
                    range_from = self.date_range.start_datetime.to_rfc3339(),
                    range_to = self.date_range.end_datetime.to_rfc3339(),
                    domain = policy.policy.policy_domain,
                    report_contact = self.contact_info.as_deref().unwrap_or("unknown"),
                    report_id = self.report_id,
                    policy_type = ?policy.policy.policy_type,
                    total_success = policy.summary.total_success,
                    total_failures = policy.summary.total_failure,
                    details = ?details,
                );
            }
        }
    }
}

impl LogReport for Feedback<'_> {
    fn log(&self) {
        tracing::warn!(
            context = "arf",
            event = "analyze",
            feedback_type = ?self.feedback_type(),
            arrival_date = DateTime::from_timestamp(self.arrival_date().unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()) as i64
            })).to_rfc3339(),
            authentication_results = ?self.authentication_results(),
            incidents = self.incidents(),
            reported_domain = ?self.reported_domain(),
            reported_uri = ?self.reported_uri(),
            reporting_mta = self.reporting_mta().unwrap_or_default(),
            source_ip = ?self.source_ip(),
            user_agent = self.user_agent().unwrap_or_default(),
            auth_failure = ?self.auth_failure(),
            delivery_result = ?self.delivery_result(),
            dkim_domain = self.dkim_domain().unwrap_or_default(),
            dkim_identity = self.dkim_identity().unwrap_or_default(),
            dkim_selector = self.dkim_selector().unwrap_or_default(),
            identity_alignment = ?self.identity_alignment(),
        );
    }
}
