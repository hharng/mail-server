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

use std::{collections::hash_map::Entry, sync::Arc, time::Duration};

use ahash::AHashMap;
use mail_auth::{
    flate2::{write::GzEncoder, Compression},
    mta_sts::{ReportUri, TlsRpt},
    report::tlsrpt::{
        DateRange, FailureDetails, Policy, PolicyDetails, PolicyType, Summary, TlsReport,
    },
};

use mail_parser::DateTime;
use reqwest::header::CONTENT_TYPE;
use std::fmt::Write;
use store::{
    write::{now, BatchBuilder, Bincode, QueueClass, ReportEvent, ValueClass},
    Deserialize, IterateParams, Serialize, ValueKey,
};

use crate::{
    config::AggregateFrequency,
    core::SMTP,
    outbound::mta_sts::{Mode, MxPattern},
    queue::RecipientDomain,
    USER_AGENT,
};

use super::{scheduler::ToHash, ReportLock, SerializedSize, TlsEvent};

#[derive(Debug, Clone)]
pub struct TlsRptOptions {
    pub record: Arc<TlsRpt>,
    pub interval: AggregateFrequency,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TlsFormat {
    rua: Vec<ReportUri>,
    policy: PolicyDetails,
    records: Vec<Option<FailureDetails>>,
}

#[cfg(feature = "test_mode")]
pub static TLS_HTTP_REPORT: parking_lot::Mutex<Vec<u8>> = parking_lot::Mutex::new(Vec::new());

impl SMTP {
    pub async fn generate_tls_report(&self, domain_name: String, events: Vec<ReportEvent>) {
        let (event_from, event_to, policy) = events
            .first()
            .map(|e| (e.seq_id, e.due, e.policy_hash))
            .unwrap();

        let span = tracing::info_span!(
            "tls-report",
            domain = domain_name,
            range_from = event_from,
            range_to = event_to,
        );

        // Deserialize report
        let config = &self.report.config.tls;
        let mut report = TlsReport {
            organization_name: self
                .eval_if(
                    &config.org_name,
                    &RecipientDomain::new(domain_name.as_str()),
                )
                .await
                .clone(),
            date_range: DateRange {
                start_datetime: DateTime::from_timestamp(event_from as i64),
                end_datetime: DateTime::from_timestamp(event_to as i64),
            },
            contact_info: self
                .eval_if(
                    &config.contact_info,
                    &RecipientDomain::new(domain_name.as_str()),
                )
                .await
                .clone(),
            report_id: format!("{}_{}", event_from, policy),
            policies: Vec::with_capacity(events.len()),
        };
        let mut rua = Vec::new();
        let mut serialized_size = serde_json::Serializer::new(SerializedSize::new(
            self.eval_if(
                &self.report.config.tls.max_size,
                &RecipientDomain::new(domain_name.as_str()),
            )
            .await
            .unwrap_or(25 * 1024 * 1024),
        ));
        let _ = serde::Serialize::serialize(&report, &mut serialized_size);

        for event in &events {
            // Deserialize report
            let tls = match self
                .shared
                .default_data_store
                .get_value::<Bincode<TlsFormat>>(ValueKey::from(ValueClass::Queue(
                    QueueClass::TlsReportHeader(event.clone()),
                )))
                .await
            {
                Ok(Some(dmarc)) => dmarc.inner,
                Ok(None) => {
                    tracing::warn!(
                        parent: &span,
                        event = "missing",
                        "Failed to read DMARC report: Report not found"
                    );
                    continue;
                }
                Err(err) => {
                    tracing::warn!(
                        parent: &span,
                        event = "error",
                        "Failed to read DMARC report: {}",
                        err
                    );
                    continue;
                }
            };
            let _ = serde::Serialize::serialize(&tls, &mut serialized_size);

            // Group duplicates
            let mut total_success = 0;
            let mut total_failure = 0;

            let from_key =
                ValueKey::from(ValueClass::Queue(QueueClass::TlsReportEvent(ReportEvent {
                    due: event.due,
                    policy_hash: event.policy_hash,
                    seq_id: 0,
                    domain: event.domain.clone(),
                })));
            let to_key =
                ValueKey::from(ValueClass::Queue(QueueClass::TlsReportEvent(ReportEvent {
                    due: event.due,
                    policy_hash: event.policy_hash,
                    seq_id: u64::MAX,
                    domain: event.domain.clone(),
                })));
            let mut record_map = AHashMap::with_capacity(tls.records.len());
            if let Err(err) = self
                .shared
                .default_data_store
                .iterate(IterateParams::new(from_key, to_key).ascending(), |_, v| {
                    if let Some(failure_details) =
                        Bincode::<Option<FailureDetails>>::deserialize(v)?.inner
                    {
                        match record_map.entry(failure_details) {
                            Entry::Occupied(mut e) => {
                                total_failure += 1;
                                *e.get_mut() += 1;
                                Ok(true)
                            }
                            Entry::Vacant(e) => {
                                if serde::Serialize::serialize(e.key(), &mut serialized_size)
                                    .is_ok()
                                {
                                    total_failure += 1;
                                    e.insert(1u32);
                                    Ok(true)
                                } else {
                                    Ok(false)
                                }
                            }
                        }
                    } else {
                        total_success += 1;
                        Ok(true)
                    }
                })
                .await
            {
                tracing::warn!(
                    parent: &span,
                    event = "error",
                    "Failed to read TLS report: {}",
                    err
                );
            }

            report.policies.push(Policy {
                policy: tls.policy,
                summary: Summary {
                    total_success,
                    total_failure,
                },
                failure_details: record_map
                    .into_iter()
                    .map(|(mut r, count)| {
                        r.failed_session_count = count;
                        r
                    })
                    .collect(),
            });

            rua = tls.rua;
        }

        if report.policies.is_empty() {
            // This should not happen
            tracing::warn!(
                parent: &span,
                event = "empty-report",
                "No policies found in report"
            );
            self.delete_tls_report(events).await;
            return;
        }

        // Compress and serialize report
        let json = report.to_json();
        let mut e = GzEncoder::new(Vec::with_capacity(json.len()), Compression::default());
        let json = match std::io::Write::write_all(&mut e, json.as_bytes()).and_then(|_| e.finish())
        {
            Ok(report) => report,
            Err(err) => {
                tracing::error!(
                    parent: &span,
                    event = "error",
                    "Failed to compress report: {}",
                    err
                );
                self.delete_tls_report(events).await;
                return;
            }
        };

        // Try delivering report over HTTP
        let mut rcpts = Vec::with_capacity(rua.len());
        for uri in &rua {
            match uri {
                ReportUri::Http(uri) => {
                    if let Ok(client) = reqwest::Client::builder()
                        .user_agent(USER_AGENT)
                        .timeout(Duration::from_secs(2 * 60))
                        .build()
                    {
                        #[cfg(feature = "test_mode")]
                        if uri == "https://127.0.0.1/tls" {
                            TLS_HTTP_REPORT.lock().extend_from_slice(&json);
                            self.delete_tls_report(events).await;
                            return;
                        }

                        match client
                            .post(uri)
                            .header(CONTENT_TYPE, "application/tlsrpt+gzip")
                            .body(json.to_vec())
                            .send()
                            .await
                        {
                            Ok(response) => {
                                if response.status().is_success() {
                                    tracing::info!(
                                        parent: &span,
                                        context = "http",
                                        event = "success",
                                        url = uri,
                                    );
                                    self.delete_tls_report(events).await;
                                    return;
                                } else {
                                    tracing::debug!(
                                        parent: &span,
                                        context = "http",
                                        event = "invalid-response",
                                        url = uri,
                                        status = %response.status()
                                    );
                                }
                            }
                            Err(err) => {
                                tracing::debug!(
                                    parent: &span,
                                    context = "http",
                                    event = "error",
                                    url = uri,
                                    reason = %err
                                );
                            }
                        }
                    }
                }
                ReportUri::Mail(mailto) => {
                    rcpts.push(mailto.as_str());
                }
            }
        }

        // Deliver report over SMTP
        if !rcpts.is_empty() {
            let from_addr = self
                .eval_if(&config.address, &RecipientDomain::new(domain_name.as_str()))
                .await
                .unwrap_or_else(|| "MAILER-DAEMON@localhost".to_string());
            let mut message = Vec::with_capacity(2048);
            let _ = report.write_rfc5322_from_bytes(
                &domain_name,
                &self
                    .eval_if(
                        &self.report.config.submitter,
                        &RecipientDomain::new(domain_name.as_str()),
                    )
                    .await
                    .unwrap_or_else(|| "localhost".to_string()),
                (
                    self.eval_if(&config.name, &RecipientDomain::new(domain_name.as_str()))
                        .await
                        .unwrap_or_else(|| "Mail Delivery Subsystem".to_string())
                        .as_str(),
                    from_addr.as_str(),
                ),
                rcpts.iter().copied(),
                &json,
                &mut message,
            );

            // Send report
            self.send_report(
                &from_addr,
                rcpts.iter(),
                message,
                &config.sign,
                &span,
                false,
            )
            .await;
        } else {
            tracing::info!(
                parent: &span,
                event = "delivery-failed",
                "No valid recipients found to deliver report to."
            );
        }
        self.delete_tls_report(events).await;
    }

    pub async fn schedule_tls(&self, event: Box<TlsEvent>) {
        let created = event.interval.to_timestamp();
        let deliver_at = created + event.interval.as_secs();
        let mut report_event = ReportEvent {
            due: deliver_at,
            policy_hash: event.policy.to_hash(),
            seq_id: created,
            domain: event.domain,
        };

        // Write policy if missing
        let mut builder = BatchBuilder::new();
        if self
            .shared
            .default_data_store
            .get_value::<()>(ValueKey::from(ValueClass::Queue(
                QueueClass::TlsReportHeader(report_event.clone()),
            )))
            .await
            .unwrap_or_default()
            .is_none()
        {
            // Serialize report
            let mut policy = PolicyDetails {
                policy_type: PolicyType::NoPolicyFound,
                policy_string: vec![],
                policy_domain: report_event.domain.clone(),
                mx_host: vec![],
            };

            match event.policy {
                super::PolicyType::Tlsa(tlsa) => {
                    policy.policy_type = PolicyType::Tlsa;
                    if let Some(tlsa) = tlsa {
                        for entry in &tlsa.entries {
                            policy.policy_string.push(format!(
                                "{} {} {} {}",
                                if entry.is_end_entity { 3 } else { 2 },
                                i32::from(entry.is_spki),
                                if entry.is_sha256 { 1 } else { 2 },
                                entry
                                    .data
                                    .iter()
                                    .fold(String::with_capacity(64), |mut s, b| {
                                        write!(s, "{b:02X}").ok();
                                        s
                                    })
                            ));
                        }
                    }
                }
                super::PolicyType::Sts(sts) => {
                    policy.policy_type = PolicyType::Sts;
                    if let Some(sts) = sts {
                        policy.policy_string.push("version: STSv1".to_string());
                        policy.policy_string.push(format!(
                            "mode: {}",
                            match sts.mode {
                                Mode::Enforce => "enforce",
                                Mode::Testing => "testing",
                                Mode::None => "none",
                            }
                        ));
                        policy
                            .policy_string
                            .push(format!("max_age: {}", sts.max_age));
                        for mx in &sts.mx {
                            let mx = match mx {
                                MxPattern::Equals(mx) => mx.to_string(),
                                MxPattern::StartsWith(mx) => format!("*.{mx}"),
                            };
                            policy.policy_string.push(format!("mx: {mx}"));
                            policy.mx_host.push(mx);
                        }
                    }
                }
                _ => (),
            }

            // Create report entry
            let entry = TlsFormat {
                rua: event.tls_record.rua.clone(),
                policy,
                records: vec![],
            };

            // Write report
            builder.set(
                ValueClass::Queue(QueueClass::TlsReportHeader(report_event.clone())),
                Bincode::new(entry).serialize(),
            );

            // Add lock
            builder.set(
                ValueClass::Queue(QueueClass::tls_lock(&report_event)),
                0u64.serialize(),
            );
        }

        // Write entry
        report_event.seq_id = self.queue.snowflake_id.generate().unwrap_or_else(now);
        builder.set(
            ValueClass::Queue(QueueClass::TlsReportEvent(report_event)),
            Bincode::new(event.failure).serialize(),
        );

        if let Err(err) = self.shared.default_data_store.write(builder.build()).await {
            tracing::error!(
                context = "report",
                event = "error",
                "Failed to write DMARC report event: {}",
                err
            );
        }
    }

    pub async fn delete_tls_report(&self, events: Vec<ReportEvent>) {
        let mut batch = BatchBuilder::new();

        for (pos, event) in events.into_iter().enumerate() {
            let from_key = ReportEvent {
                due: event.due,
                policy_hash: event.policy_hash,
                seq_id: 0,
                domain: event.domain.clone(),
            };
            let to_key = ReportEvent {
                due: event.due,
                policy_hash: event.policy_hash,
                seq_id: u64::MAX,
                domain: event.domain.clone(),
            };

            // Remove report events
            if let Err(err) = self
                .shared
                .default_data_store
                .delete_range(
                    ValueKey::from(ValueClass::Queue(QueueClass::TlsReportEvent(from_key))),
                    ValueKey::from(ValueClass::Queue(QueueClass::TlsReportEvent(to_key))),
                )
                .await
            {
                tracing::warn!(
                    context = "report",
                    event = "error",
                    "Failed to remove reports: {}",
                    err
                );
                return;
            }

            if pos == 0 {
                // Remove lock
                batch.clear(ValueClass::Queue(QueueClass::tls_lock(&event)));
            }

            // Remove report header
            batch.clear(ValueClass::Queue(QueueClass::TlsReportHeader(event)));
        }

        if let Err(err) = self.shared.default_data_store.write(batch.build()).await {
            tracing::warn!(
                context = "report",
                event = "error",
                "Failed to remove reports: {}",
                err
            );
        }
    }
}
