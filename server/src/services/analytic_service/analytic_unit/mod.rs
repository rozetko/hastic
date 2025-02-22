pub mod anomaly_analytic_unit;
pub mod pattern_analytic_unit;
pub mod threshold_analytic_unit;
pub mod types;

use self::{
    anomaly_analytic_unit::AnomalyAnalyticUnit, pattern_analytic_unit::PatternAnalyticUnit,
    threshold_analytic_unit::ThresholdAnalyticUnit, types::AnalyticUnitConfig,
};

pub fn resolve(cfg: AnalyticUnitConfig) -> Box<dyn types::AnalyticUnit + Send + Sync> {
    match cfg {
        AnalyticUnitConfig::Threshold(c) => Box::new(ThresholdAnalyticUnit::new(c.clone())),
        AnalyticUnitConfig::Pattern(c) => Box::new(PatternAnalyticUnit::new(c.clone())),
        AnalyticUnitConfig::Anomaly(c) => Box::new(AnomalyAnalyticUnit::new(c.clone())),
    }
}
