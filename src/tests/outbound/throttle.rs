/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart SMTP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::{Duration, Instant},
};

use mail_auth::MX;

use crate::{
    config::{ConfigContext, IfBlock},
    core::{Core, Session},
    queue::{manager::Queue, DeliveryAttempt, Message, QueueEnvelope},
    tests::{queue::manager::new_message, ParseTestConfig},
};

const THROTTLE: &str = "
[[queue.throttle]]
match = {if = 'sender-domain', eq = 'foobar.org'}
key = 'sender-domain'
concurrency = 1

[[queue.throttle]]
match = {if = 'sender-domain', eq = 'foobar.net'}
key = 'sender-domain'
rate = '1/30m'

[[queue.throttle]]
match = {if = 'rcpt-domain', eq = 'example.org'}
key = 'rcpt-domain'
concurrency = 1

[[queue.throttle]]
match = {if = 'rcpt-domain', eq = 'example.net'}
key = 'rcpt-domain'
rate = '1/40m'

[[queue.throttle]]
match = {if = 'mx', eq = 'mx.test.org'}
key = 'mx'
concurrency = 1

[[queue.throttle]]
match = {if = 'mx', eq = 'mx.test.net'}
key = 'mx'
rate = '1/50m'
";

#[tokio::test]
async fn throttle_outbound() {
    /*tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    )
    .unwrap();*/

    // Build test message
    let mut test_message = new_message(0);
    test_message.return_path_domain = "foobar.org".to_string();
    let mut core = Core::test();
    let mut local_qr = core.init_test_queue("smtp_throttle_outbound");
    core.session.config.rcpt.relay = IfBlock::new(true);
    core.queue.config.throttle = THROTTLE.parse_queue_throttle(&ConfigContext::default());
    core.queue.config.retry = IfBlock::new(vec![Duration::from_secs(86400)]);
    core.queue.config.notify = IfBlock::new(vec![Duration::from_secs(86400)]);
    core.queue.config.expire = IfBlock::new(Duration::from_secs(86400));

    let core = Arc::new(core);
    let mut queue = Queue::default();
    let mut session = Session::test(core.clone());
    session.data.remote_ip = "10.0.0.1".parse().unwrap();
    session.eval_session_params().await;
    session.ehlo("mx.test.org").await;
    session
        .send_message("john@foobar.org", &["bill@test.org"], "test:no_dkim", "250")
        .await;

    // Throttle sender
    let span = tracing::info_span!("test");
    let mut in_flight = vec![];
    let throttle = &core.queue.config.throttle;
    for t in &throttle.sender {
        core.queue
            .is_allowed(
                t,
                &QueueEnvelope::test(&test_message, "", ""),
                &mut in_flight,
                &span,
            )
            .await
            .unwrap();
    }
    assert!(!in_flight.is_empty());

    // Expect concurrency throttle for sender domain 'foobar.org'
    DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
        .try_deliver(core.clone(), &mut queue)
        .await;
    local_qr.assert_empty_queue();
    in_flight.clear();
    assert!(!queue.on_hold.is_empty());
    queue.next_on_hold().unwrap();

    // Expect rate limit throttle for sender domain 'foobar.net'
    test_message.return_path_domain = "foobar.net".to_string();
    for t in &throttle.sender {
        core.queue
            .is_allowed(
                t,
                &QueueEnvelope::test(&test_message, "", ""),
                &mut in_flight,
                &span,
            )
            .await
            .unwrap();
    }
    assert!(in_flight.is_empty());
    session
        .send_message("john@foobar.net", &["bill@test.org"], "test:no_dkim", "250")
        .await;
    DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
        .try_deliver(core.clone(), &mut queue)
        .await;
    local_qr.assert_empty_queue();
    assert!([1799, 1800].contains(
        &queue
            .scheduled
            .pop()
            .unwrap()
            .due
            .duration_since(Instant::now())
            .as_secs()
    ));

    // Expect concurrency throttle for recipient domain 'example.org'
    test_message.return_path_domain = "test.net".to_string();
    for t in &throttle.rcpt {
        core.queue
            .is_allowed(
                t,
                &QueueEnvelope::test(&test_message, "example.org", ""),
                &mut in_flight,
                &span,
            )
            .await
            .unwrap();
    }
    assert!(!in_flight.is_empty());
    session
        .send_message(
            "john@test.net",
            &["jane@example.org"],
            "test:no_dkim",
            "250",
        )
        .await;
    DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
        .try_deliver(core.clone(), &mut queue)
        .await;
    local_qr.read_event().await.unwrap_on_hold();
    in_flight.clear();

    // Expect rate limit throttle for recipient domain 'example.org'
    for t in &throttle.rcpt {
        core.queue
            .is_allowed(
                t,
                &QueueEnvelope::test(&test_message, "example.net", ""),
                &mut in_flight,
                &span,
            )
            .await
            .unwrap();
    }
    assert!(in_flight.is_empty());
    session
        .send_message(
            "john@test.net",
            &["jane@example.net"],
            "test:no_dkim",
            "250",
        )
        .await;
    DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
        .try_deliver(core.clone(), &mut queue)
        .await;
    assert!([2399, 2400].contains(
        &local_qr
            .read_event()
            .await
            .unwrap_retry()
            .due
            .duration_since(Instant::now())
            .as_secs()
    ));

    // Expect concurrency throttle for mx 'mx.test.org'
    core.resolvers.dns.mx_add(
        "test.org",
        vec![MX {
            exchanges: vec!["mx.test.org".to_string()],
            preference: 10,
        }],
        Instant::now() + Duration::from_secs(10),
    );
    core.resolvers.dns.ipv4_add(
        "mx.test.org",
        vec!["127.0.0.1".parse().unwrap()],
        Instant::now() + Duration::from_secs(10),
    );
    for t in &throttle.host {
        core.queue
            .is_allowed(
                t,
                &QueueEnvelope::test(&test_message, "test.org", "mx.test.org"),
                &mut in_flight,
                &span,
            )
            .await
            .unwrap();
    }
    assert!(!in_flight.is_empty());
    session
        .send_message("john@test.net", &["jane@test.org"], "test:no_dkim", "250")
        .await;
    DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
        .try_deliver(core.clone(), &mut queue)
        .await;
    local_qr.read_event().await.unwrap_on_hold();
    in_flight.clear();

    // Expect rate limit throttle for mx 'mx.test.net'
    core.resolvers.dns.mx_add(
        "test.net",
        vec![MX {
            exchanges: vec!["mx.test.net".to_string()],
            preference: 10,
        }],
        Instant::now() + Duration::from_secs(10),
    );
    core.resolvers.dns.ipv4_add(
        "mx.test.net",
        vec!["127.0.0.1".parse().unwrap()],
        Instant::now() + Duration::from_secs(10),
    );
    for t in &throttle.host {
        core.queue
            .is_allowed(
                t,
                &QueueEnvelope::test(&test_message, "example.net", "mx.test.net"),
                &mut in_flight,
                &span,
            )
            .await
            .unwrap();
    }
    assert!(in_flight.is_empty());
    session
        .send_message("john@test.net", &["jane@test.net"], "test:no_dkim", "250")
        .await;
    DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
        .try_deliver(core.clone(), &mut queue)
        .await;
    assert!([2999, 3000].contains(
        &local_qr
            .read_event()
            .await
            .unwrap_retry()
            .due
            .duration_since(Instant::now())
            .as_secs()
    ));
}

impl<'x> QueueEnvelope<'x> {
    pub fn test(message: &'x Message, domain: &'x str, mx: &'x str) -> Self {
        QueueEnvelope {
            message,
            domain,
            mx,
            remote_ip: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            local_ip: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        }
    }
}
