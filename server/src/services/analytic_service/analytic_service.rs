use std::sync::Arc;

use super::analytic_unit::types::{AnalyticUnitConfig, PatchConfig};
use super::detection_runner::DetectionRunner;
use super::types::{self, AnalyticUnitRF, DetectionRunnerConfig, LearningWaiter, HSR};
use super::{
    analytic_client::AnalyticClient,
    types::{AnalyticServiceMessage, LearningStatus, RequestType, ResponseType},
};

use crate::config::{AlertingConfig, AlertingType};
use crate::services::analytic_service::analytic_unit::resolve;
use crate::services::{
    metric_service::MetricService,
    segments_service::{self, Segment, SegmentType, SegmentsService, ID_LENGTH},
};
use crate::utils::{self};

use crate::services::analytic_service::analytic_unit::types::{AnalyticUnit, LearningResult};

use anyhow;

use tokio::sync::{mpsc, oneshot};

// TODO: now it's basically single analytic unit, service will operate on many AU
// TODO: trigger anomaly unit model update in runner
pub struct AnalyticService {
    metric_service: MetricService,
    segments_service: SegmentsService,

    alerting: Option<AlertingConfig>,

    analytic_unit: Option<AnalyticUnitRF>,
    analytic_unit_config: AnalyticUnitConfig,
    analytic_unit_learning_status: LearningStatus,

    tx: mpsc::Sender<AnalyticServiceMessage>,
    rx: mpsc::Receiver<AnalyticServiceMessage>,

    // handlers
    learning_handler: Option<tokio::task::JoinHandle<()>>,

    // awaiters
    learning_waiters: Vec<LearningWaiter>,

    detection_runner: Option<DetectionRunner>,
}

impl AnalyticService {
    pub fn new(
        metric_service: MetricService,
        segments_service: segments_service::SegmentsService,
        alerting: Option<AlertingConfig>,
    ) -> AnalyticService {
        let (tx, rx) = mpsc::channel::<AnalyticServiceMessage>(32);

        AnalyticService {
            metric_service,
            segments_service,

            alerting,

            // TODO: get it from persistance
            analytic_unit: None,
            analytic_unit_config: AnalyticUnitConfig::Pattern(Default::default()),

            analytic_unit_learning_status: LearningStatus::Initialization,
            tx,
            rx,

            // handlers
            learning_handler: None,

            // awaiters
            learning_waiters: Vec::new(),

            detection_runner: None,
        }
    }

    pub fn get_client(&self) -> AnalyticClient {
        AnalyticClient::new(self.tx.clone())
    }

    fn run_learning_waiter(&self, learning_waiter: LearningWaiter) {
        // TODO: save handler of the task
        tokio::spawn({
            let ms = self.metric_service.clone();
            let au = self.analytic_unit.as_ref().unwrap().clone();
            async move {
                match learning_waiter {
                    LearningWaiter::Detection(task) => {
                        AnalyticService::get_detections(task.sender, au, ms, task.from, task.to)
                            .await
                    }
                    LearningWaiter::HSR(task) => {
                        AnalyticService::get_hsr(task.sender, au, ms, task.from, task.to).await
                    }
                }
            }
        });
    }

    fn run_detection_runner(&mut self) {
        // TODO: handle case or make it impossible to run_detection_runner second time

        if self.analytic_unit.is_none() {
            return;
        }

        if self.analytic_unit_learning_status != LearningStatus::Ready {
            // TODO: add to waiter
            return;
        }

        let AlertingType::Webhook(acfg) = self.alerting.as_ref().unwrap().alerting_type.clone();
        let drcfg = DetectionRunnerConfig {
            endpoint: acfg.endpoint.clone(),
            interval: self.alerting.as_ref().unwrap().interval
        };
        
        let dr = DetectionRunner::new(drcfg, self.analytic_unit.as_ref().unwrap().clone());
        self.detection_runner = Some(dr);
        // dr.run();

        // TODO: create DetectionRunnerConfig from alerting
        // TODO: rerun detection runner on analytic unit change
        // if self.runner_handler.is_some() {
        //     self.runner_handler.as_mut().unwrap().abort();
        // }
        // // TODO: save handler of the task
        // self.runner_handler = Some(tokio::spawn({
        //     let au = self.analytic_unit.unwrap();
        //     let ms = self.metric_service.clone();
        //     async move {
        //         // TODO: implement
        //     }
        // }));
    }

    // TODO: maybe make `consume_request` async
    fn consume_request(&mut self, req: types::RequestType) -> () {
        match req {
            RequestType::RunLearning => {
                // TODO: if detection_runner then add it to learning_waiters
                if self.learning_handler.is_some() {
                    self.learning_handler.as_ref().unwrap().abort();
                    self.learning_handler = None;
                }
                self.learning_handler = Some(tokio::spawn({
                    self.analytic_unit_learning_status = LearningStatus::Starting;
                    let tx = self.tx.clone();
                    let ms = self.metric_service.clone();
                    let ss = self.segments_service.clone();
                    let cfg = self.analytic_unit_config.clone();
                    async move {
                        AnalyticService::run_learning(tx, cfg, ms, ss).await;
                    }
                }));
            }
            RequestType::RunDetection(task) => {
                // TODO: signle source of truth: Option<AnalyticUnit> vs LearningStatus
                if self.analytic_unit_learning_status == LearningStatus::Initialization {
                    match task
                        .sender
                        .send(Err(anyhow::format_err!("Analytics in initialization")))
                    {
                        Ok(_) => {}
                        Err(e) => {
                            println!("failed to send error about initialization");
                            println!("{:?}", e);
                        }
                    }
                    return;
                }
                if self.analytic_unit_learning_status == LearningStatus::Ready {
                    self.run_learning_waiter(LearningWaiter::Detection(task));
                } else {
                    self.learning_waiters.push(LearningWaiter::Detection(task));
                }
            }
            RequestType::GetStatus(tx) => {
                tx.send(self.analytic_unit_learning_status.clone()).unwrap();
            }

            // TODO: do it in abstract way for all analytic units
            // RequestType::GetLearningTrain(tx) => {
            //     if self.analytic_unit_learning_results.is_none() {
            //         tx.send(LearningTrain::default()).unwrap();
            //     } else {
            //         tx.send(
            //             self.analytic_unit_learning_results
            //                 .as_ref()
            //                 .unwrap()
            //                 .learning_train
            //                 .clone(),
            //         )
            //         .unwrap();
            //     }
            // }
            RequestType::GetConfig(tx) => {
                tx.send(self.analytic_unit_config.clone()).unwrap();
            }
            RequestType::PatchConfig(patch_obj, tx) => {
                self.patch_config(patch_obj, tx);
            }
            RequestType::GetHSR(task) => {
                if self.analytic_unit.is_some() {
                    self.run_learning_waiter(LearningWaiter::HSR(task));
                } else {
                    self.learning_waiters.push(LearningWaiter::HSR(task));
                }
            }
        };
    }

    // TODO: maybe make `consume_response` async
    fn consume_response(&mut self, res: types::ResponseType) {
        match res {
            // TODO: handle when learning panics
            ResponseType::LearningStarted => {
                self.analytic_unit_learning_status = LearningStatus::Learning
            }
            ResponseType::LearningFinished(results) => {
                self.learning_handler = None;
                self.analytic_unit = Some(Arc::new(tokio::sync::RwLock::new(results)));
                self.analytic_unit_learning_status = LearningStatus::Ready;

                // TODO: run tasks from self.learning_waiter
                while self.learning_waiters.len() > 0 {
                    let task = self.learning_waiters.pop().unwrap();
                    self.run_learning_waiter(task);
                }
            }
            ResponseType::LearningFinishedEmpty => {
                // TODO: drop all learning_waiters with empty results
                self.analytic_unit = None;
                self.analytic_unit_learning_status = LearningStatus::Initialization;
            }
            ResponseType::LearningDatasourceError => {
                // TODO: drop all learning_waiters with error
                self.analytic_unit = None;
                self.analytic_unit_learning_status = LearningStatus::Error;
            }
        }
    }

    fn patch_config(&mut self, patch: PatchConfig, tx: oneshot::Sender<()>) {
        let (new_conf, need_learning) = self.analytic_unit_config.patch(patch);
        self.analytic_unit_config = new_conf;
        if need_learning {
            self.consume_request(RequestType::RunLearning);
            // TODO: it's not fullu correct: we need to wait when the learning starts
            match tx.send(()) {
                Ok(_) => {}
                Err(_e) => {
                    println!("Can`t send patch config notification");
                }
            }
        } else {
            if self.analytic_unit.is_some() {
                tokio::spawn({
                    let au = self.analytic_unit.clone();
                    let cfg = self.analytic_unit_config.clone();
                    async move {
                        au.unwrap().write().await.set_config(cfg);
                        match tx.send(()) {
                            Ok(_) => {}
                            Err(_e) => {
                                println!("Can`t send patch config notification");
                            }
                        }
                    }
                });
            } else {
                match tx.send(()) {
                    Ok(_) => {}
                    Err(_e) => {
                        println!("Can`t send patch config notification");
                    }
                }
            }
        }
    }

    pub async fn serve(&mut self) {
        // TODO: remove this hack
        self.consume_request(RequestType::RunLearning);
        if self.alerting.is_some() {
            self.run_detection_runner();
        }

        while let Some(message) = self.rx.recv().await {
            match message {
                AnalyticServiceMessage::Request(req) => self.consume_request(req),
                AnalyticServiceMessage::Response(res) => self.consume_response(res),
            }
        }
    }

    async fn run_learning(
        tx: mpsc::Sender<AnalyticServiceMessage>,
        aucfg: AnalyticUnitConfig,
        ms: MetricService,
        ss: SegmentsService,
    ) {
        let mut au = resolve(aucfg);

        match tx
            .send(AnalyticServiceMessage::Response(
                ResponseType::LearningStarted,
            ))
            .await
        {
            Ok(_) => {}
            Err(_e) => println!("Fail to send learning started notification"),
        }

        // TODO: maybe to spawn_blocking here
        let lr = match au.learn(ms, ss).await {
            LearningResult::Finished => ResponseType::LearningFinished(au),
            LearningResult::DatasourceError => ResponseType::LearningDatasourceError,
            LearningResult::FinishedEmpty => ResponseType::LearningFinishedEmpty,
        };

        match tx.send(AnalyticServiceMessage::Response(lr)).await {
            Ok(_) => {}
            Err(_e) => println!("Fail to send learning results"),
        }
    }

    async fn get_detections(
        tx: oneshot::Sender<anyhow::Result<Vec<Segment>>>,
        analytic_unit: AnalyticUnitRF,
        ms: MetricService,
        from: u64,
        to: u64,
    ) {
        // It's important that we don't drop read() lock until end
        // because there mght be attempt to make .write() with setting new config
        let result = analytic_unit
            .read()
            .await
            .detect(ms, from, to)
            .await
            .unwrap();

        let result_segments: Vec<Segment> = result
            .iter()
            .map(|(p, q)| Segment {
                from: *p,
                to: *q,
                id: Some(utils::get_random_str(ID_LENGTH)),
                segment_type: SegmentType::Detection,
            })
            .collect();

        match tx.send(Ok(result_segments)) {
            Ok(_) => {}
            Err(_e) => {
                println!("failed to send results");
            }
        }
        return;
    }

    async fn get_hsr(
        tx: oneshot::Sender<anyhow::Result<HSR>>,
        analytic_unit: AnalyticUnitRF,
        ms: MetricService,
        from: u64,
        to: u64,
    ) {
        let hsr = analytic_unit
            .read()
            .await
            .get_hsr(ms, from, to)
            .await
            .unwrap();

        match tx.send(Ok(hsr)) {
            Ok(_) => {}
            Err(_e) => {
                println!("failed to send results");
            }
        }
    }
}
