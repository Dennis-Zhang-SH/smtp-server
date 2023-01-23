use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHashSet;
use mail_auth::{
    common::{parse::TxtRecordParser, verify::DomainKey},
    dkim::DomainKeyReport,
    dmarc::Dmarc,
    report::DmarcResult,
    spf::Spf,
};

use crate::{
    config::{AggregateFrequency, ConfigContext, IfBlock, List, Rate, VerifyStrategy},
    core::{Core, Session},
    tests::{session::VerifyResponse, ParseTestConfig},
};

#[tokio::test]
async fn dmarc() {
    let mut core = Core::test();
    let ctx = ConfigContext::default().parse_signatures();

    // Create temp dir for queue
    let mut qr = core.init_test_queue("smtp_dmarc_test");

    // Add SPF, DKIM and DMARC records
    core.resolvers.dns.txt_add(
        "mx.example.com",
        Spf::parse(b"v=spf1 ip4:10.0.0.1 ip4:10.0.0.2 -all").unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    core.resolvers.dns.txt_add(
        "example.com",
        Spf::parse(b"v=spf1 ip4:10.0.0.1 -all ra=spf-failures rr=e:f:s:n").unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    core.resolvers.dns.txt_add(
        "foobar.com",
        Spf::parse(b"v=spf1 ip4:10.0.0.1 -all").unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    core.resolvers.dns.txt_add(
        "ed._domainkey.example.com",
        DomainKey::parse(
            concat!(
                "v=DKIM1; k=ed25519; ",
                "p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo="
            )
            .as_bytes(),
        )
        .unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    core.resolvers.dns.txt_add(
        "default._domainkey.example.com",
        DomainKey::parse(
            concat!(
                "v=DKIM1; t=s; p=MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQ",
                "KBgQDwIRP/UC3SBsEmGqZ9ZJW3/DkMoGeLnQg1fWn7/zYt",
                "IxN2SnFCjxOCKG9v3b4jYfcTNh5ijSsq631uBItLa7od+v",
                "/RtdC2UzJ1lWT947qR+Rcac2gbto/NMqJ0fzfVjH4OuKhi",
                "tdY9tf6mcwGjaNBcWToIMmPSPDdQPNUYckcQ2QIDAQAB",
            )
            .as_bytes(),
        )
        .unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    core.resolvers.dns.txt_add(
        "_report._domainkey.example.com",
        DomainKeyReport::parse(b"ra=dkim-failures; rp=100; rr=d:o:p:s:u:v:x;").unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    core.resolvers.dns.txt_add(
        "_dmarc.example.com",
        Dmarc::parse(
            concat!(
                "v=DMARC1; p=reject; sp=quarantine; np=None; aspf=s; adkim=s; fo=1;",
                "rua=mailto:dmarc-feedback@example.com;",
                "ruf=mailto:dmarc-failures@example.com"
            )
            .as_bytes(),
        )
        .unwrap(),
        Instant::now() + Duration::from_secs(5),
    );

    // Create report channels
    let mut rr = core.init_test_report();

    let mut config = &mut core.session.config.rcpt;
    config.lookup_domains = IfBlock::new(Some(Arc::new(List::Local(AHashSet::from_iter([
        "example.com".to_string(),
    ])))));
    config.lookup_addresses = IfBlock::new(Some(Arc::new(List::Local(AHashSet::from_iter([
        "jdoe@example.com".to_string(),
    ])))));

    let mut config = &mut core.session.config;
    config.data.add_auth_results = IfBlock::new(true);
    config.data.add_date = IfBlock::new(true);
    config.data.add_message_id = IfBlock::new(true);
    config.data.add_received = IfBlock::new(true);
    config.data.add_return_path = IfBlock::new(true);
    config.data.add_received_spf = IfBlock::new(true);

    let mut config = &mut core.report.config;
    config.dkim.send = IfBlock::new(Some(Rate {
        requests: 1,
        period: Duration::from_secs(1),
    }));
    config.dmarc.send = config.dkim.send.clone();
    config.spf.send = config.dkim.send.clone();
    config.dmarc_aggregate.send = IfBlock::new(AggregateFrequency::Daily);

    let mut config = &mut core.mail_auth;
    config.spf.verify_ehlo = IfBlock::new(VerifyStrategy::Strict);
    config.spf.verify_mail_from = config.spf.verify_ehlo.clone();
    config.dkim.verify = config.spf.verify_ehlo.clone();
    config.arc.verify = config.spf.verify_ehlo.clone();
    config.dmarc.verify = config.spf.verify_ehlo.clone();

    let mut config = &mut core.report.config;
    config.spf.sign = "['rsa']"
        .parse_if::<Vec<String>>(&ctx)
        .map_if_block(&ctx.signers, "", "")
        .unwrap();
    config.dmarc.sign = config.spf.sign.clone();
    config.dkim.sign = config.spf.sign.clone();

    // SPF must pass
    let mut session = Session::test(core);
    session.data.remote_ip = "10.0.0.2".parse().unwrap();
    session.eval_session_params().await;
    session.ehlo("mx.example.com").await;
    session.mail_from("bill@example.com", "550 5.7.23").await;

    // Expect SPF auth failure report
    let message = qr.read_event().await.unwrap_message();
    assert_eq!(
        message.recipients.last().unwrap().address,
        "spf-failures@example.com"
    );
    message
        .read_lines()
        .assert_contains("DKIM-Signature: v=1; a=rsa-sha256; s=rsa; d=example.com;")
        .assert_contains("To: spf-failures@example.com")
        .assert_contains("Feedback-Type: auth-failure")
        .assert_contains("Auth-Failure: spf");

    // Second DKIM failure report should be rate limited
    session.mail_from("bill@example.com", "550 5.7.23").await;
    qr.assert_empty_queue();

    // Invalid DKIM signatures should be rejected
    session.data.remote_ip = "10.0.0.1".parse().unwrap();
    session.eval_session_params().await;
    session
        .send_message(
            "bill@example.com",
            &["jdoe@example.com"],
            "test:invalid_dkim",
            "550 5.7.20",
        )
        .await;

    // Expect DKIM auth failure report
    let message = qr.read_event().await.unwrap_message();
    assert_eq!(
        message.recipients.last().unwrap().address,
        "dkim-failures@example.com"
    );
    message
        .read_lines()
        .assert_contains("DKIM-Signature: v=1; a=rsa-sha256; s=rsa; d=example.com;")
        .assert_contains("To: dkim-failures@example.com")
        .assert_contains("Feedback-Type: auth-failure")
        .assert_contains("Auth-Failure: bodyhash");

    // Second DKIM failure report should be rate limited
    session
        .send_message(
            "bill@example.com",
            &["jdoe@example.com"],
            "test:invalid_dkim",
            "550 5.7.20",
        )
        .await;
    qr.assert_empty_queue();

    // Invalid ARC should be rejected
    session
        .send_message(
            "bill@example.com",
            &["jdoe@example.com"],
            "test:invalid_arc",
            "550 5.7.29",
        )
        .await;
    qr.assert_empty_queue();

    // Unaligned DMARC should be rejected
    session
        .send_message(
            "joe@foobar.com",
            &["jdoe@example.com"],
            "test:dkim",
            "550 5.7.1",
        )
        .await;

    // Expect DMARC auth failure report
    let message = qr.read_event().await.unwrap_message();
    assert_eq!(
        message.recipients.last().unwrap().address,
        "dmarc-failures@example.com"
    );
    message
        .read_lines()
        .assert_contains("DKIM-Signature: v=1; a=rsa-sha256; s=rsa; d=example.com;")
        .assert_contains("To: dmarc-failures@example.com")
        .assert_contains("Feedback-Type: auth-failure")
        .assert_contains("Auth-Failure: dmarc")
        .assert_contains("dmarc=fail");

    // Expect DMARC aggregate report
    let report = rr.read_report().await.unwrap_dmarc();
    assert_eq!(report.domain, "example.com");
    assert_eq!(report.interval, AggregateFrequency::Daily);
    assert_eq!(report.dmarc_record.rua().len(), 1);
    assert_eq!(report.report_record.dmarc_spf_result(), DmarcResult::Fail);

    // Second DMARC failure report should be rate limited
    session
        .send_message(
            "joe@foobar.com",
            &["jdoe@example.com"],
            "test:dkim",
            "550 5.7.1",
        )
        .await;
    qr.assert_empty_queue();

    // Messagess passing DMARC should be accepted
    session
        .send_message(
            "bill@example.com",
            &["jdoe@example.com"],
            "test:dkim",
            "250",
        )
        .await;
    qr.read_event()
        .await
        .unwrap_message()
        .read_lines()
        .assert_contains("dkim=pass")
        .assert_contains("spf=pass")
        .assert_contains("dmarc=pass")
        .assert_contains("Received-SPF: pass");
}
