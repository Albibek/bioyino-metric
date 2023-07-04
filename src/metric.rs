use std::borrow::Cow;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::fmt::Debug;

use bytes::Bytes;
use capnp::message::{Allocator, Builder, HeapAllocator};
use num_traits::{AsPrimitive, Float};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::name::{find_tag_pos, MetricName, TagFormat};
use crate::protocol_capnp::{gauge as gauge_v1, metric as cmetric_v1, metric_type};
use crate::protocol_v2_capnp::{metric as cmetric, metric::metric_meta::tags, metric::metric_value, ID as V2ID};

#[derive(Error, Debug)]
pub enum MetricError {
    #[error("float conversion")]
    FloatToRatio,

    #[error("bad sampling range")]
    Sampling,

    #[error("aggregating metrics of different types")]
    Aggregating,

    #[error("custom histogram range mismatch")]
    CustomHistrogramRange,

    #[error("usage of deprecated feature")]
    Deprecated,

    #[error("this feature is not implemented yet")]
    NotImplemented,

    #[error("decoding error: {}", _0)]
    Capnp(capnp::Error),

    #[error("schema error: {}", _0)]
    CapnpSchema(capnp::NotInSchema),

    #[error("unknown type name '{}'", _0)]
    BadTypeName(String),

    #[error("unknown protocol version '{}'", _0)]
    BadProtoVersion(String),
}

#[derive(Debug, PartialEq)]
/// This is the "view" of a metric coming from statsd as input.
///
/// While the types are same, it is different from the way it is represented internally
#[derive(Clone)]
pub enum StatsdType<F>
where
    F: Debug,
{
    Gauge(Option<i8>),
    Counter,
    Timer,
    Set,
    CustomHistogram(F, F),
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
    // TODO specilization will give us a possibility to use any other float the same way
    fn from_f64(value: f64) -> Self {
        let (mantissa, exponent, sign) = Float::integer_decode(value);
        let sign_f = f32::from(sign);
        let mantissa_f = mantissa as f32;
        let exponent_f = 2f32.powf(f32::from(exponent));
        sign_f * mantissa_f * exponent_f
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct StatsdMetric<F>
where
    F: Debug,
{
    value: F,
    mtype: StatsdType<F>,
    sampling: Option<f32>,
}

impl<F> StatsdMetric<F>
where
    F: Debug + Float,
{
    pub fn new(value: F, mtype: StatsdType<F>, sampling: Option<f32>) -> Result<Self, MetricError> {
        if let StatsdType::CustomHistogram(start, end) = mtype {
            if start >= end || !start.is_finite() || !end.is_finite() {
                return Err(MetricError::CustomHistrogramRange);
            }
        }

        Ok(Self { value, mtype, sampling })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MetricValue<F>
where
    F: Copy + PartialEq + Debug,
{
    Gauge(F),
    Counter(F),
    Timer(Vec<F>),
    Set(HashSet<u64>),
    /// Histograms store a counter for the very left bucket, and a list of buckets with their start
    /// values
    CustomHistogram(u64, Vec<(F, u64)>),
}

impl<F> MetricValue<F>
where
    F: Copy + PartialEq + Debug + Float + AsPrimitive<f64> + FromF64,
{
    /// accumulates a previously created metric data into self
    pub fn accumulate(&mut self, new: MetricValue<F>) -> Result<(), MetricError> {
        match (self, new) {
            (&mut MetricValue::Counter(ref mut value), MetricValue::Counter(new)) => {
                *value = *value + new;
            }
            (&mut MetricValue::Gauge(ref mut value), MetricValue::Gauge(new)) => {
                *value = new;
            }
            (&mut MetricValue::Timer(ref mut agg), MetricValue::Timer(ref mut agg2)) => {
                agg.append(agg2);
            }
            (&mut MetricValue::Set(ref mut hs), MetricValue::Set(ref mut hs2)) => {
                hs.extend(hs2.iter());
            }
            (&mut MetricValue::CustomHistogram(ref mut left_c1, ref mut buckets1), MetricValue::CustomHistogram(left_c2, ref buckets2)) => {
                if buckets1.len() != buckets2.len() {
                    return Err(MetricError::CustomHistrogramRange);
                }

                // for performance reasons we skip some checks here considering
                // they are not passing the constructors
                // (from_statsd_metric and for StatsdMetric::CustomHistogram)
                // the only risks are to get bad data out of them
                //
                // possible edge cases are
                // * buckets1.len() == 0 or buckets2.len() == 0 - not really an issue, meaning
                // one-bucket histogram actually
                // * +-inf/nan in floats

                // another source of error may be when someone wants to change the boundaries for metric witht the same name
                // if we blindly accept new values, we'll get the incorrect or partially incorrect data for the aggregation period
                // if we check the boundaries, we'll get incorrect data anyways, but we'll let the
                // sender know it returning error
                // as of now we prefer observability at the cost of small(intuitive assumption) performance loss
                // in some future we may change this to an option or even better: a compile-time flag
                if !buckets1.is_empty() && !buckets2.is_empty() && buckets1[0].0 != buckets2[0].0 {
                    return Err(MetricError::CustomHistrogramRange);
                }

                // we also intentionally skip the part where floats(i.e bucket boundaries) may be unequal due to
                // approximation
                //
                // here's the cases when floats may be unequal:
                // * someone uses bad float values on input using schema wrong or using non-IEEE
                // floats
                // * someone uses bioyino compiled for f32, which gives different numbers when
                // bucket ranges are counted; this case should only give a small error

                buckets1.iter_mut().zip(buckets2.iter()).map(|((_, ref mut v1), (_, v2))| *v1 += v2).last();
                *left_c1 += left_c2;
            }
            (_m1, _m2) => {
                return Err(MetricError::Aggregating);
            }
        };
        Ok(())
    }

    pub fn accumulate_statsd(&mut self, statsd: StatsdMetric<F>) -> Result<(), MetricError> {
        match (self, &statsd.mtype) {
            (MetricValue::Gauge(ref mut v), StatsdType::Gauge(Some(sign))) => {
                if *sign < 0 {
                    *v = *v - statsd.value;
                } else {
                    *v = *v + statsd.value;
                }
                Ok(())
            }
            (MetricValue::Gauge(ref mut v), StatsdType::Gauge(None)) => {
                *v = statsd.value;
                Ok(())
            }

            (MetricValue::Counter(ref mut v), StatsdType::Counter) => {
                *v = *v + statsd.value;
                Ok(())
            }
            (MetricValue::Timer(ref mut acc), StatsdType::Timer) => {
                acc.push(statsd.value);
                Ok(())
            }
            (MetricValue::Set(ref mut acc), StatsdType::Set) => {
                acc.insert(statsd.value.as_().to_bits());
                Ok(())
            }
            (MetricValue::CustomHistogram(ref mut left, ref mut buckets), StatsdType::CustomHistogram(start, end)) => {
                // check if histogram limits are valid
                if *start != buckets[0].0 || *end != buckets[buckets.len() - 1].0 {
                    return Err(MetricError::CustomHistrogramRange);
                }

                // search the first matching bucket starting from the end of all buckets
                // reverse the iteration for that, then count the right position
                match buckets.iter().rev().position(|(v, _)| statsd.value >= *v) {
                    Some(pos) => {
                        let real_pos = buckets.len() - 1 - pos;
                        buckets[real_pos].1 += 1;
                    }
                    None => *left += 1,
                }
                Ok(())
            }
            (_, _) => Err(MetricError::Aggregating),
        }
    }

    // since v1 requires separate value, we require this function to return it for further
    // setting in metric
    pub fn fill_capnp_v1<'a>(&self, builder: &mut metric_type::Builder<'a>) -> f64 {
        match self {
            MetricValue::Counter(value) => {
                builder.set_counter(());
                value.as_()
            }
            MetricValue::Gauge(value) => {
                let mut g_builder = builder.reborrow().init_gauge();
                g_builder.set_unsigned(());
                value.as_()
            }
            MetricValue::Timer(ref v) => {
                let mut timer_builder = builder.reborrow().init_timer(v.len() as u32);
                v.iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        let value: f64 = (*value).as_();
                        timer_builder.set(idx as u32, value);
                    })
                    .last();
                0f64
            }
            MetricValue::Set(ref v) => {
                let mut sebuilder = builder.reborrow().init_set(v.len() as u32);
                v.iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        sebuilder.set(idx as u32, *value);
                    })
                    .last();
                0f64
            }
            MetricValue::CustomHistogram(left, ref buckets) => {
                let mut h_builder = builder.reborrow().init_custom_histogram();
                h_builder.set_left_bucket(*left);
                let mut bucket_builder = h_builder.init_buckets(buckets.len() as u32);
                buckets
                    .iter()
                    .enumerate()
                    .map(|(idx, (boundary, counter))| {
                        let mut single_bucket = bucket_builder.reborrow().get(idx as u32);
                        single_bucket.set_value(boundary.as_());
                        single_bucket.set_counter(*counter);
                    })
                    .last();
                0f64
            }
        }
    }

    pub fn fill_capnp<'a>(&self, builder: &mut metric_value::Builder<'a>) {
        match self {
            MetricValue::Gauge(value) => builder.set_gauge(value.as_()),
            MetricValue::Counter(value) => builder.set_counter(value.as_()),
            MetricValue::Timer(ref v) => {
                let mut timer_values = builder.reborrow().init_timer(v.len() as u32);
                v.iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        let value: f64 = (*value).as_();
                        timer_values.set(idx as u32, value);
                    })
                    .last();
            }
            MetricValue::Set(ref v) => {
                let mut set_builder = builder.reborrow().init_set(v.len() as u32);
                v.iter()
                    .enumerate()
                    .map(|(idx, value)| {
                        set_builder.set(idx as u32, *value);
                    })
                    .last();
            }
            MetricValue::CustomHistogram(left, ref buckets) => {
                let mut h_builder = builder.reborrow().init_custom_histogram();
                h_builder.set_left_bucket(*left);
                let mut bucket_builder = h_builder.init_buckets(buckets.len() as u32);
                buckets
                    .iter()
                    .enumerate()
                    .map(|(idx, (boundary, counter))| {
                        let mut single_bucket = bucket_builder.reborrow().get(idx as u32);
                        single_bucket.set_value(boundary.as_());
                        single_bucket.set_counter(*counter);
                    })
                    .last();
            }
        };
    }

    pub fn from_capnp_v1(reader: metric_type::Reader, value: F) -> Result<Self, MetricError> {
        match reader.which().map_err(MetricError::CapnpSchema)? {
            metric_type::Which::Counter(()) => Ok(MetricValue::Counter(value)),
            metric_type::Which::DiffCounter(_) => Err(MetricError::Deprecated),
            metric_type::Which::Gauge(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                // since we know the protocol is only used for internal purpose
                // we can assume the signt of the gauge is not meaningful when getting it
                // from capnp message
                // this means we can replace the value in a gauge
                match reader.which().map_err(MetricError::CapnpSchema)? {
                    gauge_v1::Which::Unsigned(()) => Ok(MetricValue::Gauge(value)),
                    gauge_v1::Which::Signed(_) => Ok(MetricValue::Gauge(value)),
                }
            }
            metric_type::Which::Timer(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let mut v = Vec::new();
                v.reserve_exact(reader.len() as usize);
                reader.iter().map(|ms| v.push(FromF64::from_f64(ms))).last();
                Ok(MetricValue::Timer(v))
            }
            metric_type::Which::Set(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let v = reader.iter().collect();
                Ok(MetricValue::Set(v))
            }
            metric_type::Which::CustomHistogram(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let left = reader.get_left_bucket();
                let breader = reader.get_buckets().map_err(MetricError::Capnp)?;
                let mut buckets = Vec::new();
                buckets.reserve_exact(breader.len() as usize);
                breader.iter().map(|r| buckets.push((FromF64::from_f64(r.get_value()), r.get_counter()))).last();
                Ok(MetricValue::CustomHistogram(left, buckets))
            }
        }
    }

    pub fn from_capnp(reader: metric_value::Reader) -> Result<Self, MetricError> {
        match reader.which().map_err(MetricError::CapnpSchema)? {
            metric_value::Which::Gauge(value) => Ok(MetricValue::Gauge(F::from_f64(value))),
            metric_value::Which::Counter(value) => Ok(MetricValue::Counter(F::from_f64(value))),
            metric_value::Which::Timer(reader) => {
                let values = reader.map_err(MetricError::Capnp)?;

                let mut v = Vec::new();
                v.reserve_exact(values.len() as usize);
                values.iter().map(|ms| v.push(FromF64::from_f64(ms))).last();

                Ok(MetricValue::Timer(v))
            }
            metric_value::Which::Set(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let v = reader.iter().collect();
                Ok(MetricValue::Set(v))
            }
            metric_value::Which::CustomHistogram(reader) => {
                let reader = reader.map_err(MetricError::Capnp)?;
                let left = reader.get_left_bucket();
                let breader = reader.get_buckets().map_err(MetricError::Capnp)?;

                let mut buckets = Vec::new();
                buckets.reserve_exact(breader.len() as usize);
                breader.iter().map(|r| buckets.push((FromF64::from_f64(r.get_value()), r.get_counter()))).last();
                Ok(MetricValue::CustomHistogram(left, buckets))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// A typed, optionally timestamped metric value (i.e. without name)
pub struct Metric<F>
where
    F: Copy + PartialEq + Debug,
{
    value: MetricValue<F>,
    timestamp: Option<u64>,
    update_counter: u32,
    sampling: f32,
}

impl<F> Metric<F>
where
    F: Float + Debug + FromF64 + AsPrimitive<f64>,
{
    /// Creates a new metric
    /// Only metric type is required because it may already contain the value or many accumulated
    /// inside. If value is provided it will be accumulated according to the type used
    ///
    /// Timestamp wil be saved for later use
    pub fn new(value: MetricValue<F>, timestamp: Option<u64>, sampling: f32) -> Self {
        Self {
            value,
            timestamp,
            update_counter: 1,
            sampling,
        }
    }

    pub fn from_statsd(m: &StatsdMetric<F>, buckets: usize, timestamp: Option<u64>) -> Result<Self, MetricError> {
        let value = match m.mtype {
            StatsdType::Gauge(sign) => {
                let value = if let Some(sign) = sign {
                    if sign < 0 {
                        -m.value
                    } else {
                        m.value
                    }
                } else {
                    m.value
                };
                Ok(MetricValue::Gauge(value))
            }
            StatsdType::Counter => Ok(MetricValue::Counter(m.value)),
            StatsdType::Timer => {
                let mv = vec![m.value];
                Ok(MetricValue::Timer(mv))
            }
            StatsdType::Set => {
                let mut mhs = HashSet::with_capacity(1);
                mhs.insert(m.value.as_().to_bits());
                Ok(MetricValue::Set(mhs))
            }
            StatsdType::CustomHistogram(start, end) => {
                if buckets < 3 {
                    // buckets always have the left one, and the ones stat start with `start` and
                    // `end`
                    return Err(MetricError::CustomHistrogramRange);
                }
                let step = (end - start) / F::from_f64((buckets - 1) as f64); // (buckets - 1)  is to not count the left one
                let mut bvec = Vec::with_capacity(buckets);
                bvec.resize(buckets, (F::from_f64(0f64), 0));

                let mut bucket_found = None;
                let mut current = start;
                for i in 0..buckets {
                    if m.value >= current {
                        bucket_found = Some(i);
                    }
                    bvec[i].0 = current;

                    if i == buckets - 1 {
                        // avoid accumulating error: accept end value as is, not as accumulated
                        // step
                        current = end
                    } else {
                        current = current + step;
                    }
                }
                if let Some(idx) = bucket_found {
                    bvec[idx].1 = 1;
                    Ok(MetricValue::CustomHistogram(0, bvec))
                } else {
                    Ok(MetricValue::CustomHistogram(1, bvec))
                }
            }
        };

        Ok(Self::new(value?, timestamp, convert_sampling(&m.sampling)))
    }

    #[inline]
    pub fn updates(&self) -> F {
        F::from_f64(f64::from(self.update_counter))
    }

    #[inline]
    pub fn sampling(&self) -> F {
        F::from_f64(self.sampling as f64)
    }

    pub fn value(&self) -> &MetricValue<F> {
        &self.value
    }

    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    pub fn filter_timer(mut self) -> Self {
        if let MetricValue::Timer(ref mut agg) = self.value {
            if agg.iter().any(|f| !Float::is_finite(*f)) {

                let agg_filtered = agg.into_iter().filter_map(|f| {
                    if f.is_finite() { Some(f.clone()) } else {None}
                }).collect::<Vec<_>>();

                Self {
                    value: MetricValue::Timer(agg_filtered),
                    ..self
                }
            } else {
                self
            }
        } else {
            self
        }
    }

    pub fn sort_timer(&mut self, name: &MetricName) {
        if let MetricValue::Timer(ref mut agg) = self.value {
            agg.sort_unstable_by(|ref v1, ref v2| {
                match v1.partial_cmp(v2) {
                    None => {
                        panic!("detected uncomparable items in metric {}", name.to_string());
                    },
                    Some(r) => r,
                }
            });
        }
    }

    pub fn accumulate(&mut self, other: Metric<F>) -> Result<(), MetricError> {
        let Metric {
            value,
            timestamp,
            update_counter,
            sampling,
        } = other;
        self.update_counter += update_counter;
        if (sampling - other.sampling).abs() > f32::EPSILON {
            return Err(MetricError::Sampling);
        }
        self.timestamp = match (self.timestamp, timestamp) {
            (_, None) => self.timestamp,
            (None, Some(value)) => Some(value),
            (Some(ref value), Some(ref new)) => {
                if value > new {
                    Some(*value)
                } else {
                    Some(*new)
                }
            }
        };

        self.value.accumulate(value)
    }

    pub fn accumulate_statsd(&mut self, statsd: StatsdMetric<F>) -> Result<(), MetricError> {
        self.update_counter += 1;

        if (self.sampling - convert_sampling(&statsd.sampling)).abs() > f32::EPSILON {
            return Err(MetricError::Sampling);
        }

        self.value.accumulate_statsd(statsd)
    }

    pub fn from_capnp_v1(reader: cmetric_v1::Reader) -> Result<(MetricName, Metric<F>), MetricError> {
        let name: &[u8] = reader.get_name().map_err(MetricError::Capnp)?.as_bytes();
        let name = Bytes::copy_from_slice(name);
        let tag_pos = find_tag_pos(&name[..], TagFormat::Graphite);
        let name = MetricName::from_raw_parts(name, tag_pos);
        let value: F = F::from_f64(reader.get_value());
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

        // IMPORTANT: have this after sampling applied
        let mvalue = MetricValue::from_capnp_v1(reader.get_type().map_err(MetricError::Capnp)?, value)?;

        let timestamp = if reader.has_timestamp() {
            Some(reader.get_timestamp().map_err(MetricError::Capnp)?.get_ts())
        } else {
            None
        };

        let mut metric: Metric<F> = Metric::new(mvalue, timestamp, convert_sampling(&sampling));

        if let Some(c) = up_counter {
            metric.update_counter = c
        }

        Ok((name, metric))
    }

    pub fn from_capnp(reader: cmetric::Reader) -> Result<(MetricName, Metric<F>), MetricError> {
        let name: &[u8] = reader.get_name().map_err(MetricError::Capnp)?.as_bytes();
        let name = Bytes::copy_from_slice(name);

        let m_reader = reader.get_meta().map_err(MetricError::Capnp)?;
        let tag_pos = match m_reader.get_tags().which().map_err(MetricError::CapnpSchema)? {
            tags::Which::NoTags(()) => None,
            tags::Which::Graphite(pos) => Some(pos as usize),
        };

        let name = MetricName::from_raw_parts(name, tag_pos);

        let update_counter = m_reader.get_update_counter();

        let mv_reader = reader.get_value().map_err(MetricError::Capnp)?;
        let mvalue = MetricValue::from_capnp(mv_reader)?;

        let timestamp = if reader.has_timestamp() {
            Some(reader.get_timestamp().map_err(MetricError::Capnp)?.get_ts())
        } else {
            None
        };

        let sampling = reader.get_sampling();

        let mut metric: Metric<F> = Metric::new(mvalue, timestamp, sampling);
        metric.update_counter = update_counter;

        Ok((name, metric))
    }

    pub fn fill_capnp_v1<'a>(&self, builder: &mut cmetric_v1::Builder<'a>) {
        // no name is known at this stage

        // fill value and mtype
        let mut t_builder = builder.reborrow().init_type();
        let value: f64 = self.value.fill_capnp_v1(&mut t_builder);
        builder.set_value(value);

        // timestamp
        if let Some(timestamp) = self.timestamp {
            builder.reborrow().init_timestamp().set_ts(timestamp);
        }

        // meta
        let mut m_builder = builder.reborrow().init_meta();

        m_builder.set_update_counter(self.update_counter);
        if (self.sampling - 1f32).abs() > f32::EPSILON {
            m_builder.init_sampling().set_sampling(self.sampling);
        }
    }

    /// Fills the supplied builder, not touching the name related parts
    pub fn fill_capnp<'a>(&self, builder: &mut cmetric::Builder<'a>) {
        // fill value and mtype
        let mut v_builder = builder.reborrow().init_value();

        self.value.fill_capnp(&mut v_builder);
        // timestamp
        if let Some(timestamp) = self.timestamp {
            builder.reborrow().init_timestamp().set_ts(timestamp);
        }

        builder.set_sampling(self.sampling);

        // meta (may be initialized if fill_capnp_name was called before)
        let mut m_builder = if builder.has_meta() {
            builder.reborrow().get_meta().unwrap()
        } else {
            builder.reborrow().init_meta()
        };

        m_builder.set_update_counter(self.update_counter);
    }

    /// fills the name related parts. `unicode_checked` flag must signal that name part was
    /// already checked to be valid unicode
    pub fn fill_capnp_name<'a>(&self, builder: &mut cmetric::Builder<'a>, name: &MetricName, unicode_checked: bool) {
        // meta (may be initialized if fill_capnp was called before)
        let m_builder = if builder.has_meta() {
            builder.reborrow().get_meta().unwrap()
        } else {
            builder.reborrow().init_meta()
        };

        let mut t_builder = m_builder.init_tags();

        if let Some(pos) = name.tag_pos {
            t_builder.set_graphite(pos as u64);
        } else {
            t_builder.set_no_tags(());
        }

        let name = if unicode_checked {
            Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(&name.name) })
        } else {
            String::from_utf8_lossy(&name.name)
        };
        builder.set_name(&name);
    }

    // may be useful in future somehow
    pub fn as_capnp_v1<A: Allocator>(&self, allocator: A) -> Builder<A> {
        let mut builder = Builder::new(allocator);
        {
            let mut root = builder.init_root::<cmetric_v1::Builder>();
            self.fill_capnp_v1(&mut root);
        }
        builder
    }

    /// the boolean in name tuple should show if unicode validation was performed on name
    /// see fill_capnp_name for details
    pub fn as_capnp<A: Allocator>(&self, allocator: A, name: Option<(&MetricName, bool)>) -> Builder<A> {
        let mut builder = Builder::new(allocator);
        let mut root = builder.init_root::<cmetric::Builder>();
        self.fill_capnp(&mut root);
        if let Some((n, vu)) = name {
            self.fill_capnp_name(&mut root, n, vu);
        }
        builder
    }

    // may be useful in future somehow
    pub fn as_capnp_heap_v1(&self) -> Builder<HeapAllocator> {
        let allocator = HeapAllocator::new();
        let mut builder = Builder::new(allocator);
        {
            let mut root = builder.init_root::<cmetric_v1::Builder>();
            self.fill_capnp_v1(&mut root);
        }
        builder
    }

    /// builds a complete capnp structure
    pub fn as_capnp_heap(&self, name: Option<(&MetricName, bool)>) -> Builder<HeapAllocator> {
        let allocator = HeapAllocator::new();
        self.as_capnp(allocator, name)
    }
}

/// Metric type specification simplified to use for naming in configs etc
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(try_from = "&str")]
pub enum MetricTypeName {
    Default,
    Counter,
    Timer,
    Gauge,
    Set,
    CustomHistogram,
}

impl MetricTypeName {
    pub fn from_metric<F>(m: &Metric<F>) -> Self
    where
        F: Copy + PartialEq + Debug + Float + FromF64 + AsPrimitive<f64>,
    {
        match m.value() {
            MetricValue::Counter(_) => MetricTypeName::Counter,
            MetricValue::Timer(_) => MetricTypeName::Timer,
            MetricValue::Gauge(_) => MetricTypeName::Gauge,
            MetricValue::Set(_) => MetricTypeName::Set,
            MetricValue::CustomHistogram(_, _) => MetricTypeName::CustomHistogram,
        }
    }

    pub fn from_statsd_metric<F>(m: &StatsdMetric<F>) -> Self
    where
        F: Copy + PartialEq + Debug + Float + FromF64 + AsPrimitive<f64>,
    {
        match m.mtype {
            StatsdType::Counter => MetricTypeName::Counter,
            StatsdType::Timer => MetricTypeName::Timer,
            StatsdType::Gauge(_) => MetricTypeName::Gauge,
            StatsdType::Set => MetricTypeName::Set,
            StatsdType::CustomHistogram(_, _) => MetricTypeName::CustomHistogram,
        }
    }
}

impl TryFrom<&str> for MetricTypeName {
    type Error = MetricError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "default" => Ok(MetricTypeName::Default),
            "counter" => Ok(MetricTypeName::Counter),
            "timer" => Ok(MetricTypeName::Timer),
            "gauge" => Ok(MetricTypeName::Gauge),
            "set" => Ok(MetricTypeName::Set),
            "custom-histogram" => Ok(MetricTypeName::CustomHistogram),
            _ => Err(MetricError::BadTypeName(s.to_string())),
        }
    }
}

impl ToString for MetricTypeName {
    fn to_string(&self) -> String {
        match self {
            MetricTypeName::Default => "default",
            MetricTypeName::Counter => "counter",
            MetricTypeName::Timer => "timer",
            MetricTypeName::Gauge => "gauge",
            MetricTypeName::Set => "set",
            MetricTypeName::CustomHistogram => "custom-histogram",
        }
        .to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(try_from = "&str")]
pub enum ProtocolVersion {
    V1,
    V2,
}

impl TryFrom<&str> for ProtocolVersion {
    type Error = MetricError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "1" => Ok(ProtocolVersion::V1),
            "v1" => Ok(ProtocolVersion::V1),
            "2" => Ok(ProtocolVersion::V2),
            "v2" => Ok(ProtocolVersion::V2),
            _ => Err(MetricError::BadTypeName(s.to_string())),
        }
    }
}

impl ProtocolVersion {
    pub fn id(&self) -> u64 {
        match self {
            ProtocolVersion::V1 => 0,
            ProtocolVersion::V2 => V2ID,
        }
    }
}

#[inline]
fn convert_sampling(sampling: &Option<f32>) -> f32 {
    if let Some(s) = sampling {
        if s.is_finite() && *s < 1f32 {
            *s
        } else {
            1f32
        }
    } else {
        1f32
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use capnp::serialize::{read_message, write_message};
    type Float = f64;

    #[test]
    fn type_gauge_test() {
        let smetric = StatsdMetric::new(2f64, StatsdType::Gauge(Some(-1)), None).unwrap();
        assert_eq!(smetric.value, 2f64);
        assert_eq!(smetric.mtype, StatsdType::Gauge(Some(-1)));

        let mut metric = Metric::from_statsd(&smetric, 10, None).unwrap();
        assert_eq!(metric.value, MetricValue::Gauge(-2f64));

        let smetric = StatsdMetric::new(3f64, StatsdType::Gauge(Some(1)), None).unwrap();
        metric.accumulate_statsd(smetric).unwrap();
        assert_eq!(metric.value, MetricValue::Gauge(1f64));

        let smetric = StatsdMetric::new(42f64, StatsdType::Gauge(None), None).unwrap();
        metric.accumulate_statsd(smetric).unwrap();
        assert_eq!(metric.value, MetricValue::Gauge(42f64));
    }

    #[test]
    fn type_counter_test() {
        let smetric = StatsdMetric::new(2f64, StatsdType::Counter, Some(0.1)).unwrap();
        assert_eq!(smetric.value, 2f64);
        assert_eq!(smetric.mtype, StatsdType::Counter);

        let mut metric = Metric::from_statsd(&smetric, 10, None).unwrap();
        assert_eq!(metric.value, MetricValue::Counter(2f64));

        let smetric = StatsdMetric::new(3f64, StatsdType::Counter, Some(0.1)).unwrap();
        metric.accumulate_statsd(smetric).unwrap();
        assert_eq!(metric.value, MetricValue::Counter(5f64));
    }

    #[test]
    fn type_timer_test() {
        let smetric = StatsdMetric::new(2f64, StatsdType::Timer, Some(0.1)).unwrap();
        assert_eq!(smetric.value, 2f64);
        assert_eq!(smetric.mtype, StatsdType::Timer);

        let mut metric = Metric::from_statsd(&smetric, 10, None).unwrap();
        assert_eq!(metric.value, MetricValue::Timer(vec![2f64]));

        let smetric = StatsdMetric::new(3f64, StatsdType::Timer, Some(0.1)).unwrap();
        metric.accumulate_statsd(smetric).unwrap();
        assert_eq!(metric.value, MetricValue::Timer(vec![2f64, 3f64]));
    }

    #[test]
    fn type_set_test() {
        let smetric = StatsdMetric::new(2f64, StatsdType::Set, Some(0.1)).unwrap();
        assert_eq!(smetric.value, 2f64);
        assert_eq!(smetric.mtype, StatsdType::Set);

        let mut metric = Metric::from_statsd(&smetric, 10, None).unwrap();
        let mut expected = HashSet::new();
        expected.insert(2f64.to_bits());
        assert_eq!(metric.value, MetricValue::Set(expected.clone()));

        let smetric = StatsdMetric::new(3f64, StatsdType::Set, Some(0.1)).unwrap();
        metric.accumulate_statsd(smetric).unwrap();
        expected.insert(3f64.to_bits());
        assert_eq!(metric.value, MetricValue::Set(expected));
    }

    #[test]
    fn type_histogram_test() {
        // do not accept bad ranges
        let bad = StatsdMetric::new(2f64, StatsdType::CustomHistogram(1f64, 0f64), None);
        assert!(bad.is_err());

        let bad = StatsdMetric::new(2f64, StatsdType::CustomHistogram(1f64, f64::NAN), None);
        assert!(bad.is_err());

        // ensure sampling is considered
        let smetric = StatsdMetric::new(5f64, StatsdType::CustomHistogram(3f64, 7f64), Some(0.1)).unwrap();
        assert_eq!(smetric.value, 5f64);
        assert_eq!(smetric.mtype, StatsdType::CustomHistogram(3f64, 7f64));
        assert_eq!(smetric.sampling, Some(0.1));

        // ensure coutners and buckets are working as intended
        let smetric = StatsdMetric::new(5f64, StatsdType::CustomHistogram(3f64, 7f64), None).unwrap();
        let metric = Metric::from_statsd(&smetric, 3, None).unwrap();
        let expected = MetricValue::CustomHistogram(0, vec![(3f64, 0), (5f64, 1), (7f64, 0)]);
        assert_eq!(metric.value, expected);

        // ensure left bucket is filled
        let smetric = StatsdMetric::new(-3f64, StatsdType::CustomHistogram(3f64, 7f64), None).unwrap();
        let metric = Metric::from_statsd(&smetric, 3, None).unwrap();
        let expected = MetricValue::CustomHistogram(1, vec![(3f64, 0), (5f64, 0), (7f64, 0)]);
        assert_eq!(metric.value, expected);

        // ensure right bucket is filled
        let smetric = StatsdMetric::new(5000f64, StatsdType::CustomHistogram(3f64, 7f64), None).unwrap();
        let metric = Metric::from_statsd(&smetric, 3, None).unwrap();
        let expected = MetricValue::CustomHistogram(0, vec![(3f64, 0), (5f64, 0), (7f64, 1)]);
        assert_eq!(metric.value, expected);

        // ensure accumulation is NOT working for bad data
        let mut mvalue = MetricValue::CustomHistogram(0, vec![(3f64, 3), (5f64, 0), (7f64, 0)]);
        assert!(mvalue.accumulate(MetricValue::CustomHistogram(10, vec![(3f64, 3), (7f64, 0)])).is_err());
        assert!(mvalue
            .accumulate(MetricValue::CustomHistogram(10, vec![(2f64, 3), (4f64, 0), (6f64, 0)]))
            .is_err());

        // ensure accumulation is working
        let mut mvalue = MetricValue::CustomHistogram(0, vec![(3f64, 3), (5f64, 0), (7f64, 0)]);
        mvalue
            .accumulate(MetricValue::CustomHistogram(10, vec![(3f64, 3), (5f64, 1), (7f64, 1)]))
            .unwrap();
        let expected = MetricValue::CustomHistogram(10, vec![(3f64, 6), (5f64, 1), (7f64, 1)]);
        assert_eq!(mvalue, expected);

        // ensure accumulation from statsd works
        let smetric1 = StatsdMetric::new(6f64, StatsdType::CustomHistogram(3f64, 7f64), None).unwrap();
        let smetric2 = StatsdMetric::new(-3f64, StatsdType::CustomHistogram(3f64, 7f64), None).unwrap();
        let smetric3 = StatsdMetric::new(5000f64, StatsdType::CustomHistogram(3f64, 7f64), None).unwrap();

        let mut mvalue = MetricValue::CustomHistogram(1, vec![(3f64, 0), (5f64, 0), (7f64, 0)]);
        mvalue.accumulate_statsd(smetric1).unwrap();
        mvalue.accumulate_statsd(smetric2).unwrap();
        mvalue.accumulate_statsd(smetric3).unwrap();

        let expected = MetricValue::CustomHistogram(2, vec![(3f64, 0), (5f64, 1), (7f64, 1)]);
        assert_eq!(mvalue, expected);
    }

    fn capnp_test_v1(metric: Metric<Float>) {
        let mut buf = Vec::new();
        write_message(&mut buf, &metric.as_capnp_heap_v1()).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let reader = read_message(&mut cursor, capnp::message::DEFAULT_READER_OPTIONS).unwrap();
        let reader = reader.get_root().unwrap();
        let (_, rmetric) = Metric::<Float>::from_capnp_v1(reader).unwrap();
        assert_eq!(rmetric, metric);
    }

    fn capnp_test(metric: Metric<Float>) {
        let mut buf = Vec::new();
        write_message(&mut buf, &metric.as_capnp_heap(None)).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let reader = read_message(&mut cursor, capnp::message::DEFAULT_READER_OPTIONS).unwrap();
        let reader = reader.get_root().unwrap();
        let (_, rmetric) = Metric::<Float>::from_capnp(reader).unwrap();
        assert_eq!(rmetric, metric);
    }

    #[test]
    fn test_capnp_with_name() {
        let metric = Metric::new(MetricValue::Gauge(2f64), None, 1f32);
        let mut interm = Vec::with_capacity(128);
        interm.resize(128, 0u8);

        let tagged_name = MetricName::new("some.name.is_tagged_v2;tag1=value1;t=v".into(), TagFormat::Graphite, &mut interm).unwrap();
        let mut builder = capnp::message::Builder::new_default();
        let mut m_builder = builder.init_root::<crate::protocol_v2_capnp::metric::Builder>();

        metric.fill_capnp_name(&mut m_builder, &tagged_name, true);
        metric.fill_capnp(&mut m_builder);

        let mut buf = Vec::new();
        write_message(&mut buf, &builder).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let reader = read_message(&mut cursor, capnp::message::DEFAULT_READER_OPTIONS).unwrap();
        let reader = reader.get_root().unwrap();
        let (name, rmetric) = Metric::<Float>::from_capnp(reader).unwrap();
        dbg!(&name, &tagged_name);
        assert_eq!(name, tagged_name);
        assert_eq!(rmetric, metric);
    }

    #[test]
    fn test_metric_capnp_gauge() {
        let mut metric1 = Metric::new(MetricValue::Gauge(1f64), Some(10), 0.1);
        let metric2 = Metric::new(MetricValue::Gauge(2f64), None, 1f32);

        metric1.accumulate(metric2).unwrap();
        capnp_test_v1(metric1.clone());
        capnp_test(metric1);
    }

    #[test]
    fn test_metric_sampling_error() {
        let mut metric1 = Metric::new(MetricValue::Counter(1f64), Some(10), 0.1);
        let metric2 = Metric::new(MetricValue::Counter(2f64), None, 1f32);

        assert!(metric1.accumulate(metric2).is_ok());
    }

    #[test]
    fn test_metric_capnp_counter() {
        let mut metric1 = Metric::new(MetricValue::Counter(1f64), Some(10), 0.1);
        let metric2 = Metric::new(MetricValue::Counter(2f64), None, 0.1);

        metric1.accumulate(metric2).unwrap();
        capnp_test_v1(metric1.clone());
        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_timer() {
        let mut metric1 = Metric::new(MetricValue::Timer(vec![1f64, 2f64]), Some(10), 0.1f32);
        let metric2 = Metric::new(MetricValue::Timer(vec![3f64]), None, 0.1f32);
        metric1.accumulate(metric2).unwrap();
        capnp_test_v1(metric1.clone());
        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_set() {
        let mut set1 = HashSet::new();
        set1.extend(vec![10u64, 20u64, 10u64].into_iter());
        let mut metric1 = Metric::new(MetricValue::Set(set1), Some(10), 0.1f32);

        let mut set2 = HashSet::new();
        set2.extend(vec![10u64, 30u64].into_iter());
        let metric2 = Metric::new(MetricValue::Set(set2), None, 0.1f32);
        metric1.accumulate(metric2).unwrap();
        capnp_test_v1(metric1.clone());
        capnp_test(metric1);
    }

    #[test]
    fn test_metric_capnp_custom_histogram() {
        let mvalue = MetricValue::CustomHistogram(1, vec![(3f64, 0), (5f64, 0), (7f64, 0)]);
        let mut metric1 = Metric::new(mvalue, Some(10), 0.1f32);

        let mvalue = MetricValue::CustomHistogram(2, vec![(3f64, 1), (5f64, 1), (7f64, 0)]);
        let metric2 = Metric::new(mvalue, None, 0.1f32);
        metric1.accumulate(metric2).unwrap();
        capnp_test_v1(metric1.clone());
        capnp_test(metric1);
    }
}
