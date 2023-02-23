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

use std::time::Duration;

use crate::{
    config::ConfigContext,
    core::{Core, Session, SessionAddress},
    tests::ParseTestConfig,
};

#[tokio::test]
async fn throttle_inbound() {
    let mut core = Core::test();
    let mut config = &mut core.session.config;
    config.throttle.connect = r"[[throttle]]
    match = {if = 'remote-ip', eq = '10.0.0.1'}
    key = 'remote-ip'
    concurrency = 2
    rate = '3/1s'
    "
    .parse_throttle(&ConfigContext::default());
    config.throttle.mail_from = r"[[throttle]]
    key = 'sender'
    rate = '2/1s'
    "
    .parse_throttle(&ConfigContext::default());
    config.throttle.rcpt_to = r"[[throttle]]
    key = ['remote-ip', 'rcpt']
    rate = '2/1s'
    "
    .parse_throttle(&ConfigContext::default());

    // Test connection concurrency limit
    let mut session = Session::test(core);
    session.data.remote_ip = "10.0.0.1".parse().unwrap();
    assert!(
        session.is_allowed().await,
        "Concurrency limiter too strict."
    );
    assert!(
        session.is_allowed().await,
        "Concurrency limiter too strict."
    );
    assert!(!session.is_allowed().await, "Concurrency limiter failed.");

    // Test connection rate limit
    session.in_flight.clear(); // Manually reset concurrency limiter
    assert!(session.is_allowed().await, "Rate limiter too strict.");
    assert!(!session.is_allowed().await, "Rate limiter failed.");
    session.in_flight.clear();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        session.is_allowed().await,
        "Rate limiter did not restore quota."
    );

    // Test mail from rate limit
    session.data.mail_from = SessionAddress {
        address: "sender@test.org".to_string(),
        address_lcase: "sender@test.org".to_string(),
        domain: "test.org".to_string(),
        flags: 0,
        dsn_info: None,
    }
    .into();
    assert!(session.is_allowed().await, "Rate limiter too strict.");
    assert!(session.is_allowed().await, "Rate limiter too strict.");
    assert!(!session.is_allowed().await, "Rate limiter failed.");
    session.data.mail_from = SessionAddress {
        address: "other-sender@test.org".to_string(),
        address_lcase: "other-sender@test.org".to_string(),
        domain: "test.org".to_string(),
        flags: 0,
        dsn_info: None,
    }
    .into();
    assert!(session.is_allowed().await, "Rate limiter failed.");

    // Test recipient rate limit
    session.data.rcpt_to.push(SessionAddress {
        address: "recipient@example.org".to_string(),
        address_lcase: "recipient@example.org".to_string(),
        domain: "example.org".to_string(),
        flags: 0,
        dsn_info: None,
    });
    assert!(session.is_allowed().await, "Rate limiter too strict.");
    assert!(session.is_allowed().await, "Rate limiter too strict.");
    assert!(!session.is_allowed().await, "Rate limiter failed.");
    session.data.remote_ip = "10.0.0.2".parse().unwrap();
    assert!(session.is_allowed().await, "Rate limiter too strict.");
}
