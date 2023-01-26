use std::{io::Read, sync::Arc, time::Duration};

use mail_auth::{
    common::parse::TxtRecordParser,
    flate2::read::GzDecoder,
    mta_sts::TlsRpt,
    report::tlsrpt::{FailureDetails, PolicyType, ResultType, TlsReport},
};
use parking_lot::Mutex;

use crate::{
    config::{AggregateFrequency, ConfigContext, IfBlock},
    core::Core,
    reporting::{
        scheduler::{ReportType, Scheduler},
        tls::GenerateTlsReport,
        TlsEvent,
    },
    tests::{make_temp_dir, session::VerifyResponse, ParseTestConfig},
};

pub static TLS_HTTP_REPORT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

#[tokio::test]
async fn report_tls() {
    /*tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    )
    .unwrap();*/

    // Create scheduler
    let mut core = Core::test();
    let ctx = ConfigContext::default().parse_signatures();
    let temp_dir = make_temp_dir("smtp_report_tls_test", true);
    let config = &mut core.report.config;
    config.path = IfBlock::new(temp_dir.temp_dir.clone());
    config.hash = IfBlock::new(16);
    config.tls.sign = "['rsa']"
        .parse_if::<Vec<String>>(&ctx)
        .map_if_block(&ctx.signers, "", "")
        .unwrap();
    config.tls.max_size = IfBlock::new(4096);
    config.submitter = IfBlock::new("mx.example.org".to_string());
    config.tls.address = IfBlock::new("reports@example.org".to_string());
    config.tls.org_name = IfBlock::new("Foobar, Inc.".to_string().into());
    config.tls.contact_info = IfBlock::new("https://foobar.org/contact".to_string().into());
    let mut scheduler = Scheduler::default();

    // Create temp dir for queue
    let mut qr = core.init_test_queue("smtp_report_tls_test");
    let core = Arc::new(core);

    // Schedule TLS reports to be delivered via email
    let tls_record = Arc::new(TlsRpt::parse(b"v=TLSRPTv1;rua=mailto:reports@foobar.org").unwrap());

    for _ in 0..2 {
        // Add two successful records
        scheduler
            .schedule_tls(
                Box::new(TlsEvent {
                    domain: "foobar.org".to_string(),
                    policy: crate::reporting::PolicyType::None,
                    failure: None,
                    tls_record: tls_record.clone(),
                    interval: AggregateFrequency::Daily,
                }),
                &core,
            )
            .await;
    }

    for (policy, rt) in [
        (
            crate::reporting::PolicyType::None,
            ResultType::CertificateExpired,
        ),
        (
            crate::reporting::PolicyType::Tlsa(None),
            ResultType::TlsaInvalid,
        ),
        (
            crate::reporting::PolicyType::Sts(None),
            ResultType::StsPolicyFetchError,
        ),
        (
            crate::reporting::PolicyType::Sts(None),
            ResultType::StsPolicyInvalid,
        ),
    ] {
        scheduler
            .schedule_tls(
                Box::new(TlsEvent {
                    domain: "foobar.org".to_string(),
                    policy,
                    failure: FailureDetails::new(rt).into(),
                    tls_record: tls_record.clone(),
                    interval: AggregateFrequency::Daily,
                }),
                &core,
            )
            .await;
    }

    // Wait for flush
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(scheduler.reports.len(), 1);
    let mut report_path = Vec::new();
    match scheduler.reports.into_iter().next().unwrap() {
        (ReportType::Tls(domain), ReportType::Tls(path)) => {
            for p in &path.path {
                report_path.push(p.inner.clone());
            }
            core.generate_tls_report(domain, path);
        }
        _ => unreachable!(),
    }

    // Expect report
    let message = qr.read_event().await.unwrap_message();
    assert_eq!(
        message.recipients.last().unwrap().address,
        "reports@foobar.org"
    );
    assert_eq!(message.return_path, "reports@example.org");
    message
        .read_lines()
        .assert_contains("DKIM-Signature: v=1; a=rsa-sha256; s=rsa; d=example.com;")
        .assert_contains("To: <reports@foobar.org>")
        .assert_contains("Report Domain: foobar.org")
        .assert_contains("Submitter: mx.example.org");

    // Verify generated report
    let report = TlsReport::parse_rfc5322(message.read_message().as_bytes()).unwrap();
    assert_eq!(report.organization_name.unwrap(), "Foobar, Inc.");
    assert_eq!(report.contact_info.unwrap(), "https://foobar.org/contact");
    assert_eq!(report.policies.len(), 3);
    let mut seen = [false; 3];
    for policy in report.policies {
        match policy.policy.policy_type {
            PolicyType::Tlsa => {
                seen[0] = true;
                assert_eq!(policy.summary.total_failure, 1);
                assert_eq!(policy.summary.total_success, 0);
                assert_eq!(policy.policy.policy_domain, "foobar.org");
                assert_eq!(policy.failure_details.len(), 1);
                assert_eq!(
                    policy.failure_details.first().unwrap().result_type,
                    ResultType::TlsaInvalid
                );
            }
            PolicyType::Sts => {
                seen[1] = true;
                assert_eq!(policy.summary.total_failure, 2);
                assert_eq!(policy.summary.total_success, 0);
                assert_eq!(policy.policy.policy_domain, "foobar.org");
                assert_eq!(policy.failure_details.len(), 2);
                assert!(policy
                    .failure_details
                    .iter()
                    .any(|d| d.result_type == ResultType::StsPolicyFetchError));
                assert!(policy
                    .failure_details
                    .iter()
                    .any(|d| d.result_type == ResultType::StsPolicyInvalid));
            }
            PolicyType::NoPolicyFound => {
                seen[2] = true;
                assert_eq!(policy.summary.total_failure, 1);
                assert_eq!(policy.summary.total_success, 2);
                assert_eq!(policy.policy.policy_domain, "foobar.org");
                assert_eq!(policy.failure_details.len(), 1);
                assert_eq!(
                    policy.failure_details.first().unwrap().result_type,
                    ResultType::CertificateExpired
                );
            }
            PolicyType::Other => unreachable!(),
        }
    }

    assert!(seen[0]);
    assert!(seen[1]);
    assert!(seen[2]);

    for path in report_path {
        assert!(!path.exists());
    }

    // Schedule TLS reports to be delivered via https
    let mut scheduler = Scheduler::default();
    let tls_record = Arc::new(TlsRpt::parse(b"v=TLSRPTv1;rua=https://127.0.0.1/tls").unwrap());

    for _ in 0..2 {
        // Add two successful records
        scheduler
            .schedule_tls(
                Box::new(TlsEvent {
                    domain: "foobar.org".to_string(),
                    policy: crate::reporting::PolicyType::None,
                    failure: None,
                    tls_record: tls_record.clone(),
                    interval: AggregateFrequency::Daily,
                }),
                &core,
            )
            .await;
    }

    let mut report_path = Vec::new();
    match scheduler.reports.into_iter().next().unwrap() {
        (ReportType::Tls(domain), ReportType::Tls(path)) => {
            for p in &path.path {
                report_path.push(p.inner.clone());
            }
            core.generate_tls_report(domain, path);
        }
        _ => unreachable!(),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Uncompress report
    let gz_report = TLS_HTTP_REPORT.lock();
    let mut file = GzDecoder::new(&gz_report[..]);
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    let report = TlsReport::parse_json(&buf).unwrap();
    assert_eq!(report.organization_name.unwrap(), "Foobar, Inc.");
    assert_eq!(report.contact_info.unwrap(), "https://foobar.org/contact");
    assert_eq!(report.policies.len(), 1);

    for path in report_path {
        assert!(!path.exists());
    }
}
