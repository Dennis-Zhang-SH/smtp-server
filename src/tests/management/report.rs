use std::sync::Arc;

use ahash::{AHashMap, AHashSet, HashSet};
use mail_auth::{
    common::parse::TxtRecordParser,
    dmarc::Dmarc,
    mta_sts::TlsRpt,
    report::{
        tlsrpt::{FailureDetails, ResultType},
        ActionDisposition, DmarcResult, Record,
    },
};
use tokio::sync::mpsc;

use crate::{
    config::{AggregateFrequency, IfBlock, ServerProtocol},
    core::{management::Report, Core},
    lookup::Lookup,
    reporting::{
        scheduler::{Scheduler, SpawnReport},
        DmarcEvent, TlsEvent,
    },
    tests::{make_temp_dir, management::send_manage_request, outbound::start_test_server},
};

#[tokio::test]
#[serial_test::serial]
async fn manage_reports() {
    /*tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    )
    .unwrap();*/

    // Start reporting service
    let mut core = Core::test();
    let temp_dir = make_temp_dir("smtp_report_management_test", true);
    let config = &mut core.report.config;
    config.path = IfBlock::new(temp_dir.temp_dir.clone());
    config.hash = IfBlock::new(16);
    config.dmarc_aggregate.max_size = IfBlock::new(1024);
    config.tls.max_size = IfBlock::new(1024);
    core.queue.config.management_lookup = Arc::new(Lookup::Local(AHashSet::from_iter([
        "admin:secret".to_string(),
    ])));
    let (report_tx, report_rx) = mpsc::channel(1024);
    core.report.tx = report_tx;
    let core = Arc::new(core);
    report_rx.spawn(core.clone(), Scheduler::default());
    let _rx_manage = start_test_server(core.clone(), &[ServerProtocol::Http]);

    // Send test reporting events
    core.schedule_report(DmarcEvent {
        domain: "foobar.org".to_string(),
        report_record: Record::new()
            .with_source_ip("192.168.1.2".parse().unwrap())
            .with_action_disposition(ActionDisposition::Pass)
            .with_dmarc_dkim_result(DmarcResult::Pass)
            .with_dmarc_spf_result(DmarcResult::Fail)
            .with_envelope_from("hello@example.org")
            .with_envelope_to("other@example.org")
            .with_header_from("bye@example.org"),
        dmarc_record: Arc::new(
            Dmarc::parse(b"v=DMARC1; p=reject; rua=mailto:reports@foobar.org").unwrap(),
        ),
        interval: AggregateFrequency::Daily,
    })
    .await;
    core.schedule_report(DmarcEvent {
        domain: "foobar.net".to_string(),
        report_record: Record::new()
            .with_source_ip("a:b:c::e:f".parse().unwrap())
            .with_action_disposition(ActionDisposition::Reject)
            .with_dmarc_dkim_result(DmarcResult::Fail)
            .with_dmarc_spf_result(DmarcResult::Pass),
        dmarc_record: Arc::new(
            Dmarc::parse(
                b"v=DMARC1; p=quarantine; rua=mailto:reports@foobar.net,mailto:reports@example.net",
            )
            .unwrap(),
        ),
        interval: AggregateFrequency::Weekly,
    })
    .await;
    core.schedule_report(TlsEvent {
        domain: "foobar.org".to_string(),
        policy: crate::reporting::PolicyType::None,
        failure: None,
        tls_record: Arc::new(TlsRpt::parse(b"v=TLSRPTv1;rua=mailto:reports@foobar.org").unwrap()),
        interval: AggregateFrequency::Daily,
    })
    .await;
    core.schedule_report(TlsEvent {
        domain: "foobar.net".to_string(),
        policy: crate::reporting::PolicyType::Sts(None),
        failure: FailureDetails::new(ResultType::StsPolicyInvalid).into(),
        tls_record: Arc::new(TlsRpt::parse(b"v=TLSRPTv1;rua=mailto:reports@foobar.net").unwrap()),
        interval: AggregateFrequency::Weekly,
    })
    .await;

    // List reports
    let ids = send_manage_request::<Vec<String>>("/report/list")
        .await
        .unwrap()
        .unwrap_data();
    assert_eq!(ids.len(), 4);
    let mut id_map = AHashMap::new();
    let mut id_map_rev = AHashMap::new();
    for (report, id) in get_reports(&ids).await.into_iter().zip(ids) {
        let mut parts = id.split('!');
        let report = report.unwrap();
        let mut id_num = if parts.next().unwrap() == "t" {
            assert_eq!(report.type_, "tls");
            2
        } else {
            assert_eq!(report.type_, "dmarc");
            0
        };
        assert_eq!(parts.next().unwrap(), report.domain);
        let diff = report.range_to.to_timestamp() - report.range_from.to_timestamp();
        if report.domain == "foobar.org" {
            assert_eq!(diff, 86400);
        } else {
            assert_eq!(diff, 7 * 86400);
            id_num += 1;
        }
        id_map.insert(char::from(b'a' + id_num).to_string(), id.clone());
        id_map_rev.insert(id, char::from(b'a' + id_num).to_string());
    }

    // Test list search
    for (query, expected_ids) in [
        ("/report/list?type=dmarc", vec!["a", "b"]),
        ("/report/list?type=tls", vec!["c", "d"]),
        ("/report/list?domain=foobar.org", vec!["a", "c"]),
        ("/report/list?domain=foobar.net", vec!["b", "d"]),
        ("/report/list?domain=foobar.org&type=dmarc", vec!["a"]),
        ("/report/list?domain=foobar.net&type=tls", vec!["d"]),
    ] {
        let expected_ids = HashSet::from_iter(expected_ids.into_iter().map(|s| s.to_string()));
        let ids = send_manage_request::<Vec<String>>(query)
            .await
            .unwrap()
            .unwrap_data()
            .into_iter()
            .map(|id| id_map_rev.get(&id).unwrap().clone())
            .collect::<HashSet<_>>();
        assert_eq!(ids, expected_ids, "failed for {query}");
    }

    // Cancel reports
    for id in ["a", "b"] {
        assert_eq!(
            send_manage_request::<Vec<bool>>(&format!(
                "/report/cancel?id={}",
                id_map.get(id).unwrap(),
            ))
            .await
            .unwrap()
            .unwrap_data(),
            vec![true],
            "failed for {id}"
        );
    }
    assert_eq!(
        send_manage_request::<Vec<String>>("/report/list")
            .await
            .unwrap()
            .unwrap_data()
            .len(),
        2
    );
    let mut ids = get_reports(&[
        id_map.get("a").unwrap().clone(),
        id_map.get("b").unwrap().clone(),
        id_map.get("c").unwrap().clone(),
        id_map.get("d").unwrap().clone(),
    ])
    .await
    .into_iter();
    assert!(ids.next().unwrap().is_none());
    assert!(ids.next().unwrap().is_none());
    assert!(ids.next().unwrap().is_some());
    assert!(ids.next().unwrap().is_some());
}

async fn get_reports(ids: &[String]) -> Vec<Option<Report>> {
    send_manage_request(&format!("/report/status?id={}", ids.join(",")))
        .await
        .unwrap()
        .unwrap_data()
}
