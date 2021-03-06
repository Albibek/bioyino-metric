use std::collections::HashSet;
use std::convert::TryFrom;
use std::fmt::Debug;

use bytes::Bytes;
use capnp;
use capnp::message::{Allocator, Builder, HeapAllocator};
use num_traits::{AsPrimitive, Float};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::name::{find_tag_pos, MetricName, TagFormat};
use crate::protocol_capnp::{gauge, metric as cmetric, metric_type};

#[derive(Error, Debug)]
pub enum MetricError {
    #[error("float conversion")]
    FloatToRatio,

    #[error("bad sampling range")]
    Sampling,

    #[error("aggregating metrics of different types")]
    Aggregating,

    #[error("decoding error: {}", _0)]
    Capnp(capnp::Error),

    #[error("schema error: {}", _0)]
    CapnpSchema(capnp::NotInSchema),

    #[error("unknown type name '{}'", _0)]
    BadTypeName(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MetricType<F>
where
    F: Copy + PartialEq + Debug,
{
    Counter,
    DiffCounter(F),
    Timer(Vec<F>),
    Gauge(Option<i8>),
    Set(HashSet<u64>),
    //    Histogram,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// A typed, optionally timestamped metric value (i.e. without name)
pub struct Metric<F>
where
    F: Copy + PartialEq + Debug,
{
    pub value: F,
    pub mtype: MetricType<F>,
    pub timestamp: Option<u64>,
    pub update_counter: u32,
    pub sampling: Option<f32>,
}

pub trait FromF64 {
    fn from_f64(value: f64) -> Self;
}

impl FromF64 for f64 {
    fn from_f64(value: f64) -> Self {
        value
    }
}

impl FromF64 for f32 {
    // TODO specilaization will give us a possibility to use any other float the same way
    fn from_f64(value: f64) -> Self {
        let (mantissa, exponent, sign) = Float::integer_decode(value);
        let sign_f = f32::from(sign);
        let mantissa_f = mantissa as f32;
        let exponent_f = 2f32.powf(f32::from(exponent));
        sign_f * mantissa_f * exponent_f
    }
}

// TODO
//impl<F> Eq for Metric<F>
//F: PartialEq,
//{
//fn eq(&self, other: &Self) -> bool {
////self.self.value == other.value
//}
//}

impl<F> Metric<F>
where
    F: Float + Debug + AsPrimitive<f64> + FromF64 + Sync,
{
    pub fn new(value: F, mtype: MetricType<F>, timestamp: Option<u64>, sampling: Option<f32>) -> Result<Self, MetricError> {
        let mut metric = Metric {
            value,
            mtype,
            timestamp,
            sampling,
            update_counter: 1,
        };

        if let MetricType::Timer(ref mut agg) = metric.mtype {
            agg.push(metric.value)
        };
        if let MetricType::Set(ref mut hs) = metric.mtype {
            hs.insert(metric.value.as_().to_bits());
        };
        Ok(metric)
    }

    /// Join self with a new incoming metric depending on type
    pub fn accumulate(&mut self, new: Metric<F>) -> Result<(), MetricError> {
        use self::MetricType::*;
        self.update_counter += new.update_counter;
        match (&mut self.mtype, new.mtype) {
            (&mut Counter, Counter) => {
                self.value = self.value + new.value;
            }
            (&mut DiffCounter(ref mut previous), DiffCounter(_)) => {
                // FIXME: this is most probably incorrect when joining with another
                // non-fresh metric count2 != 1
                let prev = *previous;
                let diff = if new.value > prev { new.value - prev } else { new.value };
                *previous = new.value;
                self.value = self.value + diff;
            }
            (&mut Gauge(_), Gauge(Some(1))) => {
                self.value = self.value + new.value;
            }
            (&mut Gauge(_), Gauge(Some(-1))) => {
                self.value = self.value - new.value;
            }
            (&mut Gauge(_), Gauge(None)) => {
                self.value = new.value;
            }
            (&mut Gauge(_), Gauge(Some(_))) => {
                return Err(MetricError::Aggregating.into());
            }
            (&mut Timer(ref mut agg), Timer(ref mut agg2)) => {
                self.value = new.value;
                agg.append(agg2);
            }
            (&mut Set(ref mut hs), Set(ref mut hs2)) => {
                hs.extend(hs2.iter());
            }

            (_m1, _m2) => {
                return Err(MetricError::Aggregating.into());
            }
        };
        Ok(())
    }

    pub fn from_capnp(reader: cmetric::Reader) -> Result<(MetricName, Metric<F>), MetricError> {
        //let name: Bytes = reader.get_name().map_err(MetricError::Capnp)?.into();
        let name: &[u8] = reader.get_name().map_err(MetricError::Capnp)?.as_bytes();
        let name = Bytes::copy_from_slice(name);
        let tag_pos = find_tag_pos(&name[..], TagFormat::Graphite);
        let name = MetricName::from_raw_parts(name, tag_pos);
        let value: F = F::from_f64(reader.get_value());

        let mtype = reader.get_type().map_err(MetricError::Capnp)?;
        let mtype = match mtype.which().map_err(MetricError::CapnpSchema)? {
            metric_type::Which::Counter(()) => MetricType::Counter,
            metric_type::Which::DiffCounter(c) => MetricType::DiffCounter(F::from_f64(c)),
            metric_type::Which::Gauge(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                match reader.which().map_err(MetricError::CapnpSchema)? {
                    gauge::Which::Unsigned(()) => MetricType::Gauge(None),
                    gauge::Which::Signed(sign) => MetricType::Gauge(Some(sign)),
                }
            }
            metric_type::Which::Timer(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let mut v = Vec::new();
                v.reserve_exact(reader.len() as usize);
                reader.iter().map(|ms| v.push(FromF64::from_f64(ms))).last();
                MetricType::Timer(v)
            }
            metric_type::Which::Set(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let v = reader.iter().collect();
                MetricType::Set(v)
            }
        };

        let timestamp = if reader.has_timestamp() {
            Some(reader.get_timestamp().map_err(MetricError::Capnp)?.get_ts())
        } else {
            None
        };

        let (sampling, up_counter) = match reader.get_meta() {
            Ok(reader) => (
                if reader.has_sampling() {
                    reader.get_sampling().ok().map(|reader| reader.get_sampling())
                } else {
                    None
                },
                Some(reader.get_update_counter()),
            ),
            Err(_) => (None, None),
        };

        // we should NOT use Metric::new here because it is not a newly created metric
        // we'd get duplicate value in timer/set metrics if we used new
        let metric: Metric<F> = Metric {
            value,
            mtype,
            timestamp,
            sampling,
            update_counter: if let Some(c) = up_counter { c } else { 1 },
        };

        Ok((name, metric))
    }

    pub fn fill_capnp<'a>(&self, builder: &mut cmetric::Builder<'a>) {
        // no name is known at this stage
        // value
        builder.set_value(self.value.as_());
        // mtype
        {
            let mut t_builder = builder.reborrow().init_type();
            match self.mtype {
                MetricType::Counter => t_builder.set_counter(()),
                MetricType::DiffCounter(v) => t_builder.set_diff_counter(v.as_()),
                MetricType::Gauge(sign) => {
                    let mut g_builder = t_builder.init_gauge();
                    match sign {
                        Some(v) => g_builder.set_signed(v),
                        None => g_builder.set_unsigned(()),
                    }
                }
                MetricType::Timer(ref v) => {
                    let mut timer_builder = t_builder.init_timer(v.len() as u32);
                    v.iter()
                        .enumerate()
                        .map(|(idx, value)| {
                            let value: f64 = (*value).as_();
                            timer_builder.set(idx as u32, value);
                        })
                        .last();
                }
                MetricType::Set(ref v) => {
                    let mut set_builder = t_builder.init_set(v.len() as u32);
                    v.iter()
                        .enumerate()
                        .map(|(idx, value)| {
                            set_builder.set(idx as u32, *value);
                        })
                        .last();
                }
            }
        }

        // timestamp
        {
            if let Some(timestamp) = self.timestamp {
                builder.reborrow().init_timestamp().set_ts(timestamp);
            }
        }

        // meta
        let mut m_builder = builder.reborrow().init_meta();
        if let Some(sampling) = self.sampling {
            m_builder.reborrow().init_sampling().set_sampling(sampling)
        }
        m_builder.set_update_counter(self.update_counter);
    }

    // may be useful in future somehow
    pub fn as_capnp<A: Allocator>(&self, allocator: A) -> Builder<A> {
        let mut builder = Builder::new(allocator);
        {
            let mut root = builder.init_root::<cmetric::Builder>();
            self.fill_capnp(&mut root);
        }
        builder
    }
    // may be useful in future somehow
    pub fn as_capnp_heap(&self) -> Builder<HeapAllocator> {
        let allocator = HeapAllocator::new();
        let mut builder = Builder::new(allocator);
        {
            let mut root = builder.init_root::<cmetric::Builder>();
            self.fill_capnp(&mut root);
        }
        builder
    }
}

/// Metric type specification simplified to use for naming in configs etc
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(try_from = "&str")]
pub enum MetricTypeName {
    Default,
    Counter,
    DiffCounter,
    Timer,
    Gauge,
    Set,
}

impl MetricTypeName {
    pub fn from_metric<F>(m: &Metric<F>) -> Self
    where
        F: Copy + PartialEq + Debug,
    {
        match m.mtype {
            MetricType::Counter => MetricTypeName::Counter,
            MetricType::DiffCounter(_) => MetricTypeName::DiffCounter,
            MetricType::Timer(_) => MetricTypeName::Timer,
            MetricType::Gauge(_) => MetricTypeName::Gauge,
            MetricType::Set(_) => MetricTypeName::Set,
        }
    }
}

impl TryFrom<&str> for MetricTypeName {
    type Error = MetricError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "default" => Ok(MetricTypeName::Default),
            "counter" => Ok(MetricTypeName::Counter),
            "diff-counter" => Ok(MetricTypeName::DiffCounter),
            "timer" => Ok(MetricTypeName::Timer),
            "gauge" => Ok(MetricTypeName::Gauge),
            "set" => Ok(MetricTypeName::Set),
            _ => Err(MetricError::BadTypeName(s.to_string())),
        }
    }
}

impl ToString for MetricTypeName {
    fn to_string(&self) -> String {
        match self {
            MetricTypeName::Default => "default",
            MetricTypeName::Counter => "counter",
            MetricTypeName::DiffCounter => "diff-counter",
            MetricTypeName::Timer => "timer",
            MetricTypeName::Gauge => "gauge",
            MetricTypeName::Set => "set",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use capnp::serialize::{read_message, write_message};
    type Float = f64;

    fn capnp_test(metric: Metric<Float>) {
        let mut buf = Vec::new();
        write_message(&mut buf, &metric.as_capnp_heap()).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let reader = read_message(&mut cursor, capnp::message::DEFAULT_READER_OPTIONS).unwrap();
        let reader = reader.get_root().unwrap();
        let (_, rmetric) = Metric::<Float>::from_capnp(reader).unwrap();
        assert_eq!(rmetric, metric);
    }

    #[test]
    fn test_metric_capnp_counter() {
        let mut metric1 = Metric::new(1f64, MetricType::Counter, Some(10), Some(0.1)).unwrap();
        let metric2 = Metric::new(2f64, MetricType::Counter, None, None).unwrap();
        metric1.accumulate(metric2).unwrap();
        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_diffcounter() {
        let mut metric1 = Metric::new(1f64, MetricType::DiffCounter(0.1f64), Some(20), Some(0.2)).unwrap();
        let metric2 = Metric::new(1f64, MetricType::DiffCounter(0.5f64), None, None).unwrap();
        metric1.accumulate(metric2).unwrap();
        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_timer() {
        let mut metric1 = Metric::new(1f64, MetricType::Timer(Vec::new()), Some(10), Some(0.1)).unwrap();
        let metric2 = Metric::new(2f64, MetricType::Timer(vec![3f64]), None, None).unwrap();
        metric1.accumulate(metric2).unwrap();
        assert!(if let MetricType::Timer(ref v) = metric1.mtype { v.len() == 3 } else { false });

        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_gauge() {
        let mut metric1 = Metric::new(1f64, MetricType::Gauge(None), Some(10), Some(0.1)).unwrap();
        let metric2 = Metric::new(2f64, MetricType::Gauge(Some(-1)), None, None).unwrap();
        metric1.accumulate(metric2).unwrap();

        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_set() {
        let mut set1 = HashSet::new();
        set1.extend(vec![10u64, 20u64, 10u64].into_iter());
        let mut metric1 = Metric::new(1f64, MetricType::Set(set1), Some(10), Some(0.1)).unwrap();
        let mut set2 = HashSet::new();
        set2.extend(vec![10u64, 30u64].into_iter());
        let metric2 = Metric::new(2f64, MetricType::Set(set2), None, None).unwrap();
        metric1.accumulate(metric2).unwrap();

        capnp_test(metric1);
    }
}
