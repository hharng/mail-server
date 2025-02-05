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

use std::{fs, path::PathBuf, time::SystemTime};

use smtp_proto::{Response, RCPT_NOTIFY_DELAY, RCPT_NOTIFY_FAILURE, RCPT_NOTIFY_SUCCESS};
use store::write::now;
use utils::BlobHash;

use crate::smtp::{
    inbound::sign::TextConfigContext, ParseTestConfig, QueueReceiver, TestConfig, TestSMTP,
};
use smtp::{
    config::ConfigContext,
    core::SMTP,
    queue::{Domain, Error, ErrorDetails, HostResponse, Message, Recipient, Schedule, Status},
};

#[tokio::test]
async fn generate_dsn() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("resources");
    path.push("smtp");
    path.push("dsn");
    path.push("original.txt");
    let size = fs::metadata(&path).unwrap().len() as usize;
    let dsn_original = fs::read_to_string(&path).unwrap();

    let flags = RCPT_NOTIFY_FAILURE | RCPT_NOTIFY_DELAY | RCPT_NOTIFY_SUCCESS;
    let mut message = Message {
        size,
        id: 0,
        created: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        return_path: "sender@foobar.org".to_string(),
        return_path_lcase: "".to_string(),
        return_path_domain: "foobar.org".to_string(),
        recipients: vec![Recipient {
            domain_idx: 0,
            address: "foobar@example.org".to_string(),
            address_lcase: "foobar@example.org".to_string(),
            status: Status::PermanentFailure(HostResponse {
                hostname: ErrorDetails {
                    entity: "mx.example.org".to_string(),
                    details: "RCPT TO:<foobar@example.org>".to_string(),
                },
                response: Response {
                    code: 550,
                    esc: [5, 1, 2],
                    message: "User does not exist".to_string(),
                },
            }),
            flags: 0,
            orcpt: None,
        }],
        domains: vec![Domain {
            domain: "example.org".to_string(),
            retry: Schedule::now(),
            notify: Schedule::now(),
            expires: now() + 10,
            status: Status::TemporaryFailure(Error::ConnectionError(ErrorDetails {
                entity: "mx.domain.org".to_string(),
                details: "Connection timeout".to_string(),
            })),
            disable_tls: false,
        }],
        flags: 0,
        env_id: None,
        priority: 0,
        blob_hash: BlobHash::from(dsn_original.as_bytes()),
        quota_keys: vec![],
    };
    let span = tracing::span!(tracing::Level::INFO, "hi");

    // Load config
    let mut core = SMTP::test();
    core.shared.signers = ConfigContext::new(&[]).parse_signatures().signers;
    let config = &mut core.queue.config.dsn;
    config.sign = "\"['rsa']\"".parse_if();

    // Create temp dir for queue
    let mut qr = core.init_test_queue("smtp_dsn_test");
    qr.blob_store
        .put_blob(message.blob_hash.as_slice(), dsn_original.as_bytes())
        .await
        .unwrap();

    // Disabled DSN
    core.send_dsn(&mut message, &span).await;
    qr.assert_no_events();
    qr.assert_queue_is_empty().await;

    // Failure DSN
    message.recipients[0].flags = flags;
    core.send_dsn(&mut message, &span).await;
    let dsn_message = qr.expect_message().await;
    qr.compare_dsn(dsn_message, "failure.eml").await;

    // Success DSN
    message.recipients.push(Recipient {
        domain_idx: 0,
        address: "jane@example.org".to_string(),
        address_lcase: "jane@example.org".to_string(),
        status: Status::Completed(HostResponse {
            hostname: "mx2.example.org".to_string(),
            response: Response {
                code: 250,
                esc: [2, 1, 5],
                message: "Message accepted for delivery".to_string(),
            },
        }),
        flags,
        orcpt: None,
    });
    core.send_dsn(&mut message, &span).await;
    let dsn_message = qr.expect_message().await;
    qr.compare_dsn(dsn_message, "success.eml").await;

    // Delay DSN
    message.recipients.push(Recipient {
        domain_idx: 0,
        address: "john.doe@example.org".to_string(),
        address_lcase: "john.doe@example.org".to_string(),
        status: Status::Scheduled,
        flags,
        orcpt: "jdoe@example.org".to_string().into(),
    });
    core.send_dsn(&mut message, &span).await;
    let dsn_message = qr.expect_message().await;
    qr.compare_dsn(dsn_message, "delay.eml").await;

    // Mixed DSN
    for rcpt in &mut message.recipients {
        rcpt.flags = flags;
    }
    message.domains[0].notify.due = now();
    core.send_dsn(&mut message, &span).await;
    let dsn_message = qr.expect_message().await;
    qr.compare_dsn(dsn_message, "mixed.eml").await;

    // Load queue
    let queue = qr.read_queued_messages().await;
    assert_eq!(queue.len(), 4);
}

impl QueueReceiver {
    async fn compare_dsn(&self, message: Message, test: &str) {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("resources");
        path.push("smtp");
        path.push("dsn");
        path.push(test);

        let bytes = self
            .blob_store
            .get_blob(message.blob_hash.as_slice(), 0..u32::MAX)
            .await
            .unwrap()
            .unwrap();

        let dsn = remove_ids(bytes);
        let dsn_expected = fs::read_to_string(&path).unwrap();

        if dsn != dsn_expected {
            let mut failed = PathBuf::from(&path);
            failed.set_extension("failed");
            fs::write(&failed, dsn.as_bytes()).unwrap();
            panic!(
                "Failed for {}, output saved to {}",
                path.display(),
                failed.display()
            );
        }
    }
}

fn remove_ids(message: Vec<u8>) -> String {
    let old_message = String::from_utf8(message).unwrap();
    let mut message = String::with_capacity(old_message.len());
    let mut found_dkim = false;
    let mut skip = false;

    let mut boundary = "";
    for line in old_message.split("\r\n") {
        if skip {
            if line.chars().next().unwrap().is_ascii_whitespace() {
                continue;
            } else {
                skip = false;
            }
        }
        if line.starts_with("Date:") || line.starts_with("Message-ID:") {
            continue;
        } else if !found_dkim && line.starts_with("DKIM-Signature:") {
            found_dkim = true;
            skip = true;
            continue;
        } else if line.starts_with("--") {
            message.push_str(&line.replace(boundary, "mime_boundary"));
        } else if let Some((_, boundary_)) = line.split_once("boundary=\"") {
            boundary = boundary_.split_once('"').unwrap().0;
            message.push_str(&line.replace(boundary, "mime_boundary"));
        } else if line.starts_with("Arrival-Date:") {
            message.push_str("Arrival-Date: <date goes here>");
        } else if line.starts_with("Will-Retry-Until:") {
            message.push_str("Will-Retry-Until: <date goes here>");
        } else {
            message.push_str(line);
        }
        message.push_str("\r\n");
    }

    if !found_dkim {
        panic!("No DKIM signature found in: {old_message}");
    }

    message
}
