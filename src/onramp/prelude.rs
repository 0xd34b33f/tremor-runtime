// Copyright 2018-2020, Wayfair GmbH
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub(crate) use crate::codec::{self, Codec};
pub(crate) use crate::errors::*;
pub(crate) use crate::metrics::RampReporter;
pub(crate) use crate::onramp::{self, Onramp};
pub(crate) use crate::pipeline;
pub(crate) use crate::preprocessor::{self, Preprocessors};
pub(crate) use crate::system::METRICS_PIPELINE;
pub(crate) use crate::url::TremorURL;
pub(crate) use crate::utils::{hostname, nanotime, ConfigImpl};
pub(crate) use async_std::sync::{channel, Receiver};
pub(crate) use async_std::task;
pub(crate) use simd_json::json;
pub(crate) use tremor_pipeline::EventOriginUri;

// TODO pub here too?
use std::mem;
pub(crate) use std::thread;

pub fn make_preprocessors(preprocessors: &[String]) -> Result<Preprocessors> {
    preprocessors
        .iter()
        .map(|n| preprocessor::lookup(&n))
        .collect()
}

pub fn handle_pp(
    preprocessors: &mut Preprocessors,
    ingest_ns: &mut u64,
    data: Vec<u8>,
) -> Result<Vec<Vec<u8>>> {
    let mut data = vec![data];
    let mut data1 = Vec::new();
    for pp in preprocessors {
        data1.clear();
        for (i, d) in data.iter().enumerate() {
            match pp.process(ingest_ns, d) {
                Ok(mut r) => data1.append(&mut r),
                Err(e) => {
                    error!("Preprocessor[{}] error {}", i, e);
                    return Err(e);
                }
            }
        }
        mem::swap(&mut data, &mut data1);
    }
    Ok(data)
}

// We are borrowing a dyn box as we don't want to pass ownership.
#[allow(
    clippy::borrowed_box,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
pub(crate) fn send_event(
    pipelines: &[(TremorURL, pipeline::Addr)],
    preprocessors: &mut Preprocessors,
    codec: &mut Box<dyn Codec>,
    metrics_reporter: &mut RampReporter,
    ingest_ns: &mut u64,
    origin_uri: &tremor_pipeline::EventOriginUri,
    id: u64,
    data: Vec<u8>,
) {
    if let Ok(data) = handle_pp(preprocessors, ingest_ns, data) {
        for d in data {
            match codec.decode(d, *ingest_ns) {
                Ok(Some(data)) => {
                    metrics_reporter.periodic_flush(*ingest_ns);
                    metrics_reporter.increment_out();

                    let event = tremor_pipeline::Event {
                        is_batch: false,
                        id,
                        data,
                        ingest_ns: *ingest_ns,
                        // TODO make origin_uri non-optional here too?
                        origin_uri: Some(origin_uri.clone()),
                        kind: None,
                    };

                    let len = pipelines.len();

                    for (input, addr) in &pipelines[0..len - 1] {
                        if let Some(input) = input.instance_port() {
                            if let Err(e) = addr.addr.send(pipeline::Msg::Event {
                                input: input.into(),
                                event: event.clone(),
                            }) {
                                error!("[Onramp] failed to send to pipeline: {}", e);
                            }
                        }
                    }

                    let (input, addr) = &pipelines[len - 1];
                    if let Some(input) = input.instance_port() {
                        if let Err(e) = addr.addr.send(pipeline::Msg::Event {
                            input: input.into(),
                            event,
                        }) {
                            error!("[Onramp] failed to send to pipeline: {}", e);
                        }
                    }
                }
                Ok(None) => (),
                Err(e) => {
                    metrics_reporter.increment_error();
                    error!("[Codec] {}", e);
                }
            }
        }
    } else {
        // record preprocessor failures too
        metrics_reporter.increment_error();
    };
}

pub(crate) enum PipeHandlerResult {
    Terminate,
    Retry,
    Normal,
}

// Handles pipeline connections for an onramp
pub(crate) async fn handle_pipelines(
    rx: &Receiver<onramp::Msg>,
    pipelines: &mut Vec<(TremorURL, pipeline::Addr)>,
    metrics_reporter: &mut RampReporter,
) -> Result<PipeHandlerResult> {
    if pipelines.is_empty() {
        let msg = rx.recv().await?;
        handle_pipelines_msg(msg, pipelines, metrics_reporter)
    } else if rx.is_empty() {
        Ok(PipeHandlerResult::Normal)
    } else {
        let msg = rx.recv().await?;
        handle_pipelines_msg(msg, pipelines, metrics_reporter)
    }
}

pub(crate) fn handle_pipelines_msg(
    msg: onramp::Msg,
    pipelines: &mut Vec<(TremorURL, pipeline::Addr)>,
    metrics_reporter: &mut RampReporter,
) -> Result<PipeHandlerResult> {
    if pipelines.is_empty() {
        match msg {
            onramp::Msg::Connect(ps) => {
                for p in &ps {
                    if p.0 == *METRICS_PIPELINE {
                        metrics_reporter.set_metrics_pipeline(p.clone());
                    } else {
                        pipelines.push(p.clone());
                    }
                }
                Ok(PipeHandlerResult::Retry)
            }
            onramp::Msg::Disconnect { tx, .. } => {
                tx.send(true)?;
                Ok(PipeHandlerResult::Terminate)
            }
        }
    } else {
        match msg {
            onramp::Msg::Connect(mut ps) => pipelines.append(&mut ps),
            onramp::Msg::Disconnect { id, tx } => {
                pipelines.retain(|(pipeline, _)| pipeline != &id);
                if pipelines.is_empty() {
                    tx.send(true)?;
                    return Ok(PipeHandlerResult::Terminate);
                } else {
                    tx.send(false)?;
                }
            }
        };
        Ok(PipeHandlerResult::Normal)
    }
}
