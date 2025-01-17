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

use std::sync::Arc;

use ahash::AHashSet;
use smtp_proto::{AUTH_LOGIN, AUTH_PLAIN};

use crate::{
    config::ConfigContext,
    core::{Core, Session, State},
    lookup::Lookup,
    tests::{session::VerifyResponse, ParseTestConfig},
};

#[tokio::test]
async fn auth() {
    let mut core = Core::test();
    let mut ctx = ConfigContext::default();
    ctx.lookup.insert(
        "plain".to_string(),
        Arc::new(Lookup::Local(AHashSet::from_iter([
            "john:secret".to_string(),
            "jane:p4ssw0rd".to_string(),
        ]))),
    );

    let mut config = &mut core.session.config.auth;

    config.require = r"[{if = 'remote-ip', eq = '10.0.0.1', then = true},
    {else = false}]"
        .parse_if(&ctx);
    config.lookup = r"[{if = 'remote-ip', eq = '10.0.0.1', then = 'plain'},
    {else = false}]"
        .parse_if::<Option<String>>(&ctx)
        .map_if_block(&ctx.lookup, "", "")
        .unwrap();
    config.errors_max = r"[{if = 'remote-ip', eq = '10.0.0.1', then = 2},
    {else = 3}]"
        .parse_if(&ctx);
    config.errors_wait = "'100ms'".parse_if(&ctx);
    config.mechanisms = format!(
        "[{{if = 'remote-ip', eq = '10.0.0.1', then = {}}},
    {{else = 0}}]",
        AUTH_PLAIN | AUTH_LOGIN
    )
    .as_str()
    .parse_if(&ctx);
    core.session.config.extensions.future_release =
        r"[{if = 'authenticated-as', ne = '', then = '1d'},
    {else = false}]"
            .parse_if(&ConfigContext::default());

    // EHLO should not avertise plain text auth without TLS
    let mut session = Session::test(core);
    session.data.remote_ip = "10.0.0.1".parse().unwrap();
    session.eval_session_params().await;
    session.stream.tls = false;
    session
        .ehlo("mx.foobar.org")
        .await
        .assert_not_contains(" PLAIN")
        .assert_not_contains(" LOGIN");

    // EHLO should advertise AUTH for 10.0.0.1
    session.stream.tls = true;
    session
        .ehlo("mx.foobar.org")
        .await
        .assert_contains("AUTH ")
        .assert_contains(" PLAIN")
        .assert_contains(" LOGIN")
        .assert_not_contains("FUTURERELEASE");

    // Invalid password should be rejected
    session
        .cmd("AUTH PLAIN AGpvaG4AY2hpbWljaGFuZ2Fz", "535 5.7.8")
        .await;

    // Session should be disconnected after second invalid auth attempt
    session
        .ingest(b"AUTH PLAIN AGpvaG4AY2hpbWljaGFuZ2Fz\r\n")
        .await
        .unwrap_err();
    session.response().assert_code("421 4.3.0");

    // Should not be able to send without authenticating
    session.state = State::default();
    session.mail_from("bill@foobar.org", "503 5.5.1").await;

    // Successful PLAIN authentication
    session.data.auth_errors = 0;
    session
        .cmd("AUTH PLAIN AGpvaG4Ac2VjcmV0", "235 2.7.0")
        .await;
    session.mail_from("bill@foobar.org", "250").await;
    session.data.mail_from.take();

    // Should not be able to authenticate twice
    session
        .cmd("AUTH PLAIN AGpvaG4Ac2VjcmV0", "503 5.5.1")
        .await;

    // FUTURERELEASE extension should be available after authenticating
    session
        .ehlo("mx.foobar.org")
        .await
        .assert_not_contains("AUTH ")
        .assert_not_contains(" PLAIN")
        .assert_not_contains(" LOGIN")
        .assert_contains("FUTURERELEASE 86400");

    // Successful LOGIN authentication
    session.data.authenticated_as.clear();
    session.cmd("AUTH LOGIN", "334").await;
    session.cmd("amFuZQ==", "334").await;
    session.cmd("cDRzc3cwcmQ=", "235 2.7.0").await;

    // Login should not be advertised to 10.0.0.2
    session.data.remote_ip = "10.0.0.2".parse().unwrap();
    session.eval_session_params().await;
    session.stream.tls = true;
    session
        .ehlo("mx.foobar.org")
        .await
        .assert_not_contains("AUTH ")
        .assert_not_contains(" PLAIN")
        .assert_not_contains(" LOGIN");
    session
        .cmd("AUTH PLAIN AGpvaG4Ac2VjcmV0", "503 5.5.1")
        .await;
}
