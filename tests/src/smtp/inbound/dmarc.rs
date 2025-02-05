/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
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
    sync::Arc,
    time::{Duration, Instant},
};

use directory::core::config::ConfigDirectory;
use mail_auth::{
    common::{parse::TxtRecordParser, verify::DomainKey},
    dkim::DomainKeyReport,
    dmarc::Dmarc,
    report::DmarcResult,
    spf::Spf,
};
use store::Store;
use utils::config::{if_block::IfBlock, Config};

use crate::smtp::{
    inbound::{dummy_stores, sign::TextConfigContext, TestMessage, TestReportingEvent},
    session::{TestSession, VerifyResponse},
    ParseTestConfig, TestConfig, TestSMTP,
};
use smtp::{
    config::{AggregateFrequency, ConfigContext, VerifyStrategy},
    core::{Session, SMTP},
};

const DIRECTORY: &str = r#"
[storage]
lookup = "dummy"

[directory."local"]
type = "memory"

[[directory."local".principals]]
name = "john"
description = "John Doe"
secret = "secret"
email = ["jdoe@example.com"]

"#;

#[tokio::test]
async fn dmarc() {
    let mut core = SMTP::test();
    core.shared.signers = ConfigContext::new(&[]).parse_signatures().signers;

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
    core.shared.directories = Config::new(DIRECTORY)
        .unwrap()
        .parse_directory(&dummy_stores(), Store::default())
        .await
        .unwrap()
        .directories;
    let config = &mut core.session.config.rcpt;
    config.directory = IfBlock::new("local".to_string());

    let config = &mut core.session.config;
    config.data.add_auth_results = IfBlock::new(true);
    config.data.add_date = IfBlock::new(true);
    config.data.add_message_id = IfBlock::new(true);
    config.data.add_received = IfBlock::new(true);
    config.data.add_return_path = IfBlock::new(true);
    config.data.add_received_spf = IfBlock::new(true);

    let config = &mut core.report.config;
    config.dkim.send = "\"[1, 1s]\"".parse_if();
    config.dmarc.send = config.dkim.send.clone();
    config.spf.send = config.dkim.send.clone();
    config.dmarc_aggregate.send = IfBlock::new(AggregateFrequency::Daily);

    let config = &mut core.mail_auth;
    config.spf.verify_ehlo = r#"[{if = "remote_ip = '10.0.0.2'", then = 'strict'},
    { else = 'relaxed' }]"#
        .parse_if_constant::<VerifyStrategy>();
    config.spf.verify_mail_from = config.spf.verify_ehlo.clone();
    config.dmarc.verify = IfBlock::new(VerifyStrategy::Strict);
    config.arc.verify = config.dmarc.verify.clone();
    config.dkim.verify = r#"[{if = "sender_domain = 'test.net'", then = 'relaxed'},
    { else = 'strict' }]"#
        .parse_if_constant::<VerifyStrategy>();

    let config = &mut core.report.config;
    config.spf.sign = "\"['rsa']\"".parse_if();
    config.dmarc.sign = "\"['rsa']\"".parse_if();
    config.dkim.sign = "\"['rsa']\"".parse_if();

    // SPF must pass
    let core = Arc::new(core);
    let mut session = Session::test(core.clone());
    session.data.remote_ip_str = "10.0.0.2".to_string();
    session.data.remote_ip = session.data.remote_ip_str.parse().unwrap();
    session.eval_session_params().await;
    session.ehlo("mx.example.com").await;
    session.mail_from("bill@example.com", "550 5.7.23").await;

    // Expect SPF auth failure report
    let message = qr.expect_message().await;
    assert_eq!(
        message.recipients.last().unwrap().address,
        "spf-failures@example.com"
    );
    message
        .read_lines(&qr)
        .await
        .assert_contains("DKIM-Signature: v=1; a=rsa-sha256; s=rsa; d=example.com;")
        .assert_contains("To: spf-failures@example.com")
        .assert_contains("Feedback-Type: auth-failure")
        .assert_contains("Auth-Failure: spf");

    // Second DKIM failure report should be rate limited
    session.mail_from("bill@example.com", "550 5.7.23").await;
    qr.assert_no_events();

    // Invalid DKIM signatures should be rejected
    session.data.remote_ip_str = "10.0.0.1".to_string();
    session.data.remote_ip = session.data.remote_ip_str.parse().unwrap();
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
    let message = qr.expect_message().await;
    assert_eq!(
        message.recipients.last().unwrap().address,
        "dkim-failures@example.com"
    );
    message
        .read_lines(&qr)
        .await
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
    qr.assert_no_events();

    // Invalid ARC should be rejected
    session
        .send_message(
            "bill@example.com",
            &["jdoe@example.com"],
            "test:invalid_arc",
            "550 5.7.29",
        )
        .await;
    qr.assert_no_events();

    // Unaligned DMARC should be rejected
    core.resolvers.dns.txt_add(
        "test.net",
        Spf::parse(b"v=spf1 -all").unwrap(),
        Instant::now() + Duration::from_secs(5),
    );
    session
        .send_message(
            "joe@test.net",
            &["jdoe@example.com"],
            "test:invalid_dkim",
            "550 5.7.1",
        )
        .await;

    // Expect DMARC auth failure report
    let message = qr.expect_message().await;
    assert_eq!(
        message.recipients.last().unwrap().address,
        "dmarc-failures@example.com"
    );
    message
        .read_lines(&qr)
        .await
        .assert_contains("DKIM-Signature: v=1; a=rsa-sha256; s=rsa; d=example.com;")
        .assert_contains("To: dmarc-failures@example.com")
        .assert_contains("Feedback-Type: auth-failure")
        .assert_contains("Auth-Failure: dmarc")
        .assert_contains("dmarc=3Dnone");

    // Expect DMARC aggregate report
    let report = rr.read_report().await.unwrap_dmarc();
    assert_eq!(report.domain, "example.com");
    assert_eq!(report.interval, AggregateFrequency::Daily);
    assert_eq!(report.dmarc_record.rua().len(), 1);
    assert_eq!(report.report_record.dmarc_spf_result(), DmarcResult::Fail);

    // Second DMARC failure report should be rate limited
    session
        .send_message(
            "joe@test.net",
            &["jdoe@example.com"],
            "test:invalid_dkim",
            "550 5.7.1",
        )
        .await;
    qr.assert_no_events();

    // Messagess passing DMARC should be accepted
    session
        .send_message(
            "bill@example.com",
            &["jdoe@example.com"],
            "test:dkim",
            "250",
        )
        .await;
    qr.expect_message()
        .await
        .read_lines(&qr)
        .await
        .assert_contains("dkim=pass")
        .assert_contains("spf=pass")
        .assert_contains("dmarc=pass")
        .assert_contains("Received-SPF: pass");
}
