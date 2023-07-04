use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use num_traits::{AsPrimitive, Float};
use serde::{Deserialize, Serialize};

use crate::MetricName;
use crate::metric::{FromF64, Metric, MetricTypeName, MetricValue};

/// Percentile counter. Not safe against all edge cases:
///
/// * requires at least two elements in vector
/// * vector MUST be sorted
pub fn percentile<F>(vec: &[F], nth: F) -> F
where
    F: Float + AsPrimitive<usize>,
{
    let last = F::from(vec.len() - 1).unwrap(); // usize to float should be ok for both f32 and f64
    if last == F::zero() {
        return vec[0];
    }

    let k = nth * last;
    let f = k.floor();
    let c = k.ceil();

    if c == f {
        // exact nth percentile have been found
        return vec[k.as_()];
    }

    let m0 = c - k;
    let m1 = k - f;
    let d0 = vec[f.as_()] * m0;
    let d1 = vec[c.as_()] * m1;
    d0 + d1
}

fn fill_cached_sum<F>(agg: &[F], sum: &mut Option<F>)
where
    F: Float,
{
    if sum.is_none() {
        let first = if let Some(first) = agg.first() {
            first
        } else {
            return;
        };
        *sum = Some(agg.iter().skip(1).fold(*first, |acc, &v| acc + v))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, try_from = "String")]
/// Contains list of all possible aggregates and some additional data.
pub enum Aggregate<F>
where
    F: Copy + Float + Debug + FromF64 + AsPrimitive<usize>,
{
    /// an aggregate for single valued metrics like gauges and counters
    Value,
    Count,
    Last,
    Min,
    Max,
    Sum,
    Median,
    Mean,
    UpdateCount,

    /// A number of updates per second, must be in seconds
    /// NOTE: aggretate value may not be set out of the box, i.e. when converted from string
    Rate(Option<F>),

    /// The Nth percentile aggregate, second value must match the oringinal value from config
    /// for proper string conversion, see source code for more details
    // user will want the exact same number formatting of percentile like in config, but
    // float converstions from/to string may lose it, so
    // we prefer to keep the percentile as is, with the original integer parsed from config value
    // this is also VERY useful when comparing and hashing percentiles too
    // the only one downside is that i.e. 0.8th and 0.800 th percentile will be different metrics,
    // but same values, but this is easily acceptable and should be really very rare case
    Percentile(F, u64),

    /// Nth bucket for histograms, the value is unset when converted from string
    /// and must be set explicitly when aggregated, considering a correct number of buckets
    /// known externally
    Bucket(Option<usize>),
}

impl<F> TryFrom<String> for Aggregate<F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize>,
{
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "value" => Ok(Aggregate::Value),
            "count" => Ok(Aggregate::Count),
            "last" => Ok(Aggregate::Last),
            "min" => Ok(Aggregate::Min),
            "max" => Ok(Aggregate::Max),
            "sum" => Ok(Aggregate::Sum),
            "median" => Ok(Aggregate::Median),
            "mean" => Ok(Aggregate::Mean),
            "updates" => Ok(Aggregate::UpdateCount),
            "rate" => Ok(Aggregate::Rate(None)),
            s if s.starts_with("percentile-") => {
                // check in match guarantees minus char exists
                let pos = s.chars().position(|c| c == '-').unwrap() + 1;
                let num: u64 = u64::from_str(&s[pos..]).map_err(|_| "percentile value is not unsigned integer".to_owned())?;
                let mut divider = 10f64;

                let numf = num as f64;
                // divider is f64, so it's always bigger than u64:MAX and therefore never
                // overflow
                while numf > divider {
                    divider *= 10.0;
                }

                Ok(Aggregate::Percentile(F::from_f64(numf / divider), num))
            }
            "bucket" => Ok(Aggregate::Bucket(None)),
            _ => Err("unknown aggregate name".into()),
        }
    }
}

impl<F> ToString for Aggregate<F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize>,
{
    fn to_string(&self) -> String {
        match self {
            Aggregate::Value => "".to_string(),
            Aggregate::Count => "count".to_string(),
            Aggregate::Last => "last".to_string(),
            Aggregate::Min => "min".to_string(),
            Aggregate::Max => "max".to_string(),
            Aggregate::Sum => "sum".to_string(),
            Aggregate::Median => "median".to_string(),
            Aggregate::Mean => "mean".to_string(),
            Aggregate::UpdateCount => "updates".to_string(),
            Aggregate::Rate(_) => "rate".to_string(),
            Aggregate::Percentile(p, _) if !p.is_finite() => "bad_percentile".to_string(),
            Aggregate::Percentile(_, num) => format!("percentile.{}", num),
            Aggregate::Bucket(None) => "bad_bucket".to_string(),
            Aggregate::Bucket(Some(nth)) => format!("bucket.{}", nth),
        }
    }
}

impl<F> Hash for Aggregate<F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize>,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Aggregate::Value => 0usize.hash(state),
            Aggregate::Count => 1usize.hash(state),
            Aggregate::Last => 2usize.hash(state),
            Aggregate::Min => 3usize.hash(state),
            Aggregate::Max => 4usize.hash(state),
            Aggregate::Sum => 5usize.hash(state),
            Aggregate::Median => 6usize.hash(state),
            Aggregate::Mean => 7usize.hash(state),
            Aggregate::UpdateCount => 8usize.hash(state),
            Aggregate::Rate(ref r) => {
                9usize.hash(state);
                // we hash F as integer decoded value
                r.map(Float::integer_decode).hash(state);
            }
            // we need this for hashing and comparison, so we just use a value different from other
            // enum values
            // the second thing we need here is correctness, so nobody could send us some strange
            // percentile value like inf or nan (maybe there will be no such case, but just for the
            // sake of correctness we'd better do this
            Aggregate::Percentile(p, _) if !p.is_finite() => 11usize.hash(state), //std::usize::MAX,
            Aggregate::Percentile(_, num) => {
                12usize.hash(state);
                num.hash(state);
            }
            Aggregate::Bucket(nth) => {
                13usize.hash(state);
                nth.hash(state);
            }
        }
    }
}

impl<F> Eq for Aggregate<F> where F: Float + Debug + FromF64 + AsPrimitive<usize> {}

// to be consistent with hasher, we do the comparison in the same way
impl<F> PartialEq for Aggregate<F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize>,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Aggregate::Value, Aggregate::Value) => true,
            (Aggregate::Count, Aggregate::Count) => true,
            (Aggregate::Last, Aggregate::Last) => true,
            (Aggregate::Min, Aggregate::Min) => true,
            (Aggregate::Max, Aggregate::Max) => true,
            (Aggregate::Sum, Aggregate::Sum) => true,
            (Aggregate::Median, Aggregate::Median) => true,
            (Aggregate::Mean, Aggregate::Mean) => true,
            (Aggregate::UpdateCount, Aggregate::UpdateCount) => true,
            (Aggregate::Rate(r1), Aggregate::Rate(r2)) => r1 == r2,
            // we need this for hashing and comparison, so we just use a value different from other
            // percentile value like inf or nan (maybe there will be no such case, but just for the
            // sake of correctness we'd better do this
            (Aggregate::Percentile(_, num), Aggregate::Percentile(_, o)) => num == o,
            (Aggregate::Bucket(b), Aggregate::Bucket(o)) => b == o,
            _ => false,
        }
    }
}

impl<F> Aggregate<F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize> + AsPrimitive<f64>,
{
    /// calculates the corresponding aggregate from the input metric
    /// returns None in all inapplicable cases, like zero-length vector or metric type mismatch
    /// cached_sum must relate to the same metric between calls, giving incorrect results or
    /// panics otherwise
    pub fn calculate(&self, metric: &Metric<F>, cached_sum: &mut Option<F>, timer_last: Option<F>) -> Option<F> {
        match (metric.value(), self) {
            // for sets calculate only count
            (MetricValue::Set(ref hs), &Aggregate::Count) => Some(F::from_f64(hs.len() as f64) / metric.sampling()),
            // don't count values for timers and sets
            (MetricValue::Set(_), &Aggregate::Value) => None,
            (MetricValue::Timer(_), &Aggregate::Value) => None,
            // for timers calculate all aggregates
            (MetricValue::Timer(ref agg), &s) => match s {
                Aggregate::Value => None,
                Aggregate::Count => {
                    let len = F::from_f64(agg.len() as f64);
                    Some(len / metric.sampling())
                }
                Aggregate::Last => timer_last,
                Aggregate::Min => Some(agg[0]),
                Aggregate::Max => Some(agg[agg.len() - 1]),
                Aggregate::Sum => {
                    fill_cached_sum(agg, cached_sum);
                    cached_sum.map(|sum| sum / metric.sampling())
                }
                Aggregate::Median => Some(percentile(agg, F::from_f64(0.5))),
                Aggregate::Mean => {
                    // the case with len = 0 and sum != None is real here, but we intentinally let it
                    // panic on division by zero to get incorrect usage from code to be explicit
                    fill_cached_sum(agg, cached_sum);
                    cached_sum.map(|sum| {
                        let len = F::from_f64(agg.len() as f64);
                        // for mean we don't divide to sampling because len should be also divided
                        // and this double division can be eliminated
                        sum / len
                    })
                }
                Aggregate::UpdateCount => Some(metric.updates()),
                Aggregate::Rate(Some(secs)) => Some(metric.updates() / secs / metric.sampling()),
                Aggregate::Rate(None) => None,
                Aggregate::Percentile(ref p, _) => Some(percentile(agg, *p)),
                Aggregate::Bucket(_) => None,
            },
            (MetricValue::CustomHistogram(left, buckets), &Aggregate::Bucket(Some(nth))) => {
                let value = if nth == 0 {
                    // index 0 corresponds for left bucket...
                    Some(F::from_f64(*left as f64))
                } else if nth <= buckets.len() {
                    // this means other indexes must be shifted left
                    Some(F::from_f64(buckets[nth - 1].1 as f64))
                } else {
                    None
                };

                value.map(|value| value / metric.sampling())
            }
            // Histogram + any other type except update and rate goes to last catch-all
            // buckets are exclusive for histograms
            (_, &Aggregate::Bucket(_)) => None,
            // count value for applicable types
            (MetricValue::Gauge(v), &Aggregate::Value) => Some(*v),
            (MetricValue::Counter(v), &Aggregate::Value) => Some(*v / metric.sampling()),

            // for other types calculate only update counter
            (_, &Aggregate::UpdateCount) => Some(metric.updates()),
            (_, &Aggregate::Rate(Some(secs))) => Some(metric.updates() / secs / metric.sampling()),
            _ => None,
        }.and_then(|value| {
            // filter away NaNs and infinities
            if value.is_finite() { Some(value) } else { None }
        })
    }
}

/// A state for calculating all aggregates over metric
/// Implements iterator returning the index of aggregate in the input and the aggregate value
/// if such value should exist for an aggregate
pub struct AggregateCalculator<'a, F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize>,
{
    metric: &'a Metric<F>,
    timer_sum: Option<F>,
    timer_last: Option<F>,
    aggregates: &'a [Aggregate<F>],
    current: usize,
}

impl<'a, F> AggregateCalculator<'a, F>
where
    F: Float + Debug + FromF64 + AsPrimitive<f64> + AsPrimitive<usize>,
{
    pub fn new(metric: &'a mut Metric<F>, aggregates: &'a [Aggregate<F>], name: &MetricName) -> Self {
        let timer_last = if let MetricValue::Timer(ref agg) = metric.value() {
            if let Some(last) = agg.last() {
                Some(*last)
            } else {
                None
            }
        } else {
            None
        };

        let timer_sum = if let MetricValue::Timer(ref agg) = metric.value() {
            let first = agg.first().unwrap();
            Some(agg.iter().skip(1).fold(*first, |acc, &v| acc + v))
        } else {
            None
        };

        metric.sort_timer(name);
        Self {
            metric,
            timer_sum,
            timer_last,
            aggregates,
            current: 0,
        }
    }
}

impl<'a, F> Iterator for AggregateCalculator<'a, F>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize> + AsPrimitive<f64>,
{
    type Item = Option<(usize, F)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.aggregates.len() {
            return None;
        }

        let agg = &self.aggregates[self.current];
        let calc = agg
            .calculate(self.metric, &mut self.timer_sum, self.timer_last)
            .map(|result| (self.current, result));
        self.current += 1;
        Some(calc)
    }
}

/// A helper function giving all possible aggregates for each metric type name.
/// Includes ony one, 99th percentile for the sake of complenetes
/// `interval` paremeter is only used to set the rate aggregation interval
pub fn possible_aggregates<F>(interval: Option<F>, buckets: Option<usize>) -> HashMap<MetricTypeName, Vec<Aggregate<F>>>
where
    F: Float + Debug + FromF64 + AsPrimitive<usize>,
{
    let mut map = HashMap::new();
    map.insert(MetricTypeName::Counter, vec![Aggregate::Value, Aggregate::UpdateCount]);

    map.insert(
        MetricTypeName::Timer,
        vec![
        Aggregate::Count,
        Aggregate::Last,
        Aggregate::Min,
        Aggregate::Max,
        Aggregate::Sum,
        Aggregate::Median,
        Aggregate::Mean,
        Aggregate::UpdateCount,
        Aggregate::Rate(interval),
        Aggregate::Percentile(F::from_f64(0.99), 99),
        ],
    );
    if let Some(num) = buckets {
        let mut v = Vec::with_capacity(num);
        for i in 0..num {
            v.push(Aggregate::Bucket(Some(i)))
        }
        map.insert(MetricTypeName::CustomHistogram, v);
    }
    map.insert(MetricTypeName::Gauge, vec![Aggregate::Value, Aggregate::UpdateCount]);
    map.insert(MetricTypeName::Set, vec![Aggregate::Count, Aggregate::UpdateCount]);
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::metric::{StatsdMetric, StatsdType};
    use std::collections::{HashMap, HashSet};

    #[test]
    fn aggregates_eq_and_hashing_f32() {
        let c32: Aggregate<f32> = Aggregate::Count;
        assert!(c32 != Aggregate::Min);

        assert!(Aggregate::Percentile(0.75f32, 75) != Aggregate::Percentile(0.999, 999));

        let mut hm = HashMap::new();
        // ensure hashing works good for typical percentiles up to 99999
        for p in 1..100_000u32 {
            hm.insert(Aggregate::Percentile(1f32 / p as f32, p as u64), ());
            hm.insert(Aggregate::Percentile(1f32 / p as f32, p as u64), ());
        }
        assert_eq!(hm.len(), 100000 - 1);
    }

    #[test]
    fn aggregates_eq_and_hashing_f64() {
        let c64: Aggregate<f64> = Aggregate::Count;
        assert!(c64 != Aggregate::Min);

        assert!(Aggregate::Percentile(0.75f64, 75) != Aggregate::Percentile(0.999f64, 999));

        let mut hm = HashMap::new();
        // ensure hashing works good for typical percentiles up to 99999
        for p in 1..100_000u32 {
            hm.insert(Aggregate::Percentile(1f64 / f64::from(p), p as u64), ());
            hm.insert(Aggregate::Percentile(1f64 / f64::from(p), p as u64), ());
        }
        assert_eq!(hm.len(), 100000 - 1);
    }

    #[test]
    fn percentile_to_string() {
        assert_eq!(&Aggregate::Percentile(0.75f64, 75).to_string(), "percentile.75");
        assert_eq!(&Aggregate::Percentile(0.009f64, 9).to_string(), "percentile.9");
        assert_eq!(&Aggregate::Percentile(0.8f64, 80).to_string(), "percentile.80");
        assert_eq!(&Aggregate::Percentile(0.800f64, 800).to_string(), "percentile.800");
    }

    // a little helper for easier aggregation testing
    struct TestData {
        samples: Vec<f64>,
        num_samples: f64,
        to_aggregate: Vec<Metric<f64>>,
        expected: HashMap<Aggregate<f64>, Vec<(Metric<f64>, f64)>>,
        seconds: f64,
        buckets: usize,
        rate: f64,
        rate_agg: Aggregate<f64>,
    }

    impl TestData {
        fn new(sampling: f32) -> Self {
            let seconds = 30f64;
            let samples = vec![12f64, 43f64, 1f64, 9f64, 84f64, 55f64, 31f64, 16f64, 64f64];
            let sampling = sampling as f64;
            let num_samples = (samples.len()) as f64;

            Self {
                samples,
                num_samples,
                expected: HashMap::new(),
                to_aggregate: Vec::new(),
                seconds,
                buckets: 5,
                rate: (num_samples + 1f64) / seconds / sampling,
                rate_agg: Aggregate::Rate(Some(seconds)),
            }
        }
    }

    // This function generates all possible aggregates(only one percentile obviously)
    // and ensures the td.to_aggregate aggregates exactly to td.expected without any
    // additional or missing aggregates
    fn test_aggregation(td: TestData) {
        // create aggregates list, this also tests all aggregates are parsed
        let aggregates = vec![
            "count",
            "min",
            "updates",
            "max",
            "sum",
            "median",
            "rate",
            "percentile-85",
            "last",
            "mean",
            "bucket",
        ];
        let mut aggregates = aggregates
            .into_iter()
            .map(|s| Aggregate::try_from(s.to_string()).unwrap())
            .collect::<Vec<Aggregate<f64>>>();

        // 6th element must be rate
        // which we explicitly set to aggregation_interval value because we cannot get it from
        // just string parsing
        if let &mut Aggregate::Rate(ref mut r @ None) = &mut aggregates[6] {
            *r = Some(td.seconds)
        } else {
            panic!("6th element must be rate, got {:?}, check the test", aggregates[6]);
        }

        // buckets have to be added as separate aggregate each
        // we check the parsing first and pop the empty one
        if aggregates.pop() != Some(Aggregate::Bucket(None)) {
            panic!("9th (last) element must be empty bucket, check the test");
        }

        // then push the required ones (+1 is for left bucket)
        for i in 0..(td.buckets + 1) {
            aggregates.push(Aggregate::Bucket(Some(i)))
        }

        // add hidden value aggregator to the head
        aggregates.insert(0, Aggregate::Value);

        let mut results = HashMap::new();
        td.to_aggregate
            .into_iter()
            .map(|metric| {
                let mut calc_metric = metric.clone();
                let calculator = AggregateCalculator::new(&mut calc_metric, &aggregates);
                calculator
                    //.inspect(|res| {
                    //dbg!(res);
                    //})
                    // count all of them that are countable (filtering None) and leaving the aggregate itself
                    .filter_map(|result| result)
                    // set corresponding name
                    .map(|(idx, value)| {
                        // it would be better to use metric as a key, but it doesn't implement Eq,
                        // so atm we trick it this way
                        if !results.contains_key(&aggregates[idx]) {
                            results.insert(aggregates[idx].clone(), Vec::new());
                        }
                        results.get_mut(&aggregates[idx]).unwrap().push((metric.clone(), value));
                    })
                .last();
                })
        .last();

        //dbg!(&expected, &results);
        if td.expected.len() != results.len() {
            panic!(
                "expected len({}) does not match results len({})\n expected: {:?}\n got:\n{:?}",
                td.expected.len(),
                results.len(),
                td.expected,
                results
            );
        }
        assert_eq!(td.expected.len(), results.len(), "expected len does not match results len");
        for (ec, ev) in &td.expected {
            //dbg!(ec);
            let rv = results.get(ec).expect("expected key not found in results");
            assert_eq!(ev.len(), rv.len(), "have \n {:?} \n expect \n {:?}", ev, rv);
            // for percentile and a rate a value can be a bit not equal
            //if ec == &Aggregate::Percentile(0.85f64, 85) || ec == &td.rate_agg {
            ev.iter()
                .zip(rv.iter())
                .map(|((_, e), (_, r))| {
                    let diff = (e - r).abs();
                    assert!(diff < 0.0001, "(expected){} ~= {}: {}", e, r, diff);
                })
            .last();
            //} else {
            //assert_eq!(ev, rv, "\non {:?} expected: \n {:?} \n not equal to \n {:?}", &ec, &ev, &rv);
            //}
            }
    }

    #[test]
    fn aggregate_gauge() {
        let mut td = TestData::new(1.);

        let mut gauge = Metric::new(MetricValue::Gauge(1f64), None, 1.);
        td.samples
            .iter()
            .map(|t| {
                let gauge2 = Metric::new(MetricValue::Gauge(*t as f64), None, 1.);
                gauge.accumulate(gauge2).unwrap();
            })
        .last();

        td.to_aggregate.push(gauge.clone());

        let num_samples = td.samples.len() as f64;
        // gauges only consider last value, but default aggregates still exist
        td.expected.insert(Aggregate::Value, vec![(gauge.clone(), 64f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(gauge.clone(), num_samples + 1f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(gauge.clone(), td.rate)]);
        test_aggregation(td);
    }

    #[test]
    fn aggregate_gauge_sampled() {
        let mut td = TestData::new(0.1);

        let mut gauge = Metric::new(MetricValue::Gauge(1f64), None, 0.1);
        td.samples
            .iter()
            .map(|t| {
                let gauge2 = Metric::new(MetricValue::Gauge(*t as f64), None, 0.1);
                gauge.accumulate(gauge2).unwrap();
            })
        .last();

        td.to_aggregate.push(gauge.clone());

        let num_samples = td.samples.len() as f64;
        // gauges only consider last value, but default aggregates still exist
        td.expected.insert(Aggregate::Value, vec![(gauge.clone(), 64f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(gauge.clone(), num_samples + 1f64)]);
        dbg!(td.rate);
        td.expected.insert(td.rate_agg.clone(), vec![(gauge.clone(), td.rate)]);
        test_aggregation(td);
    }

    #[test]
    fn aggregate_counter() {
        let mut td = TestData::new(1.);
        // counters must be aggregated into 3 aggregates: value, update counter and rate
        let mut counter = Metric::new(MetricValue::Counter(1f64), None, 1.);
        let mut sign = 1f64;
        td.samples
            .iter()
            .map(|t| {
                sign = -sign;
                let counter2 = Metric::new(MetricValue::Counter(*t * sign), None, 1.);
                counter.accumulate(counter2).unwrap();
            })
        .last();

        td.to_aggregate.push(counter.clone());

        // aggregated value for counter is a sum of all incoming data considering signs
        td.expected.insert(Aggregate::Value, vec![(counter.clone(), -68f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(counter.clone(), td.num_samples + 1f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(counter.clone(), td.rate)]);

        test_aggregation(td);
    }

    #[test]
    fn aggregate_counter_sampled() {
        let mut td = TestData::new(0.1);
        // counters must be aggregated into 3 aggregates: value, update counter and rate
        let mut counter = Metric::new(MetricValue::Counter(1f64), None, 0.1);
        let mut sign = 1f64;
        td.samples
            .iter()
            .map(|t| {
                sign = -sign;
                let counter2 = Metric::new(MetricValue::Counter(*t * sign), None, 0.1);
                counter.accumulate(counter2).unwrap();
            })
        .last();

        td.to_aggregate.push(counter.clone());

        // aggregated value for counter is a sum of all incoming data considering signs
        td.expected.insert(Aggregate::Value, vec![(counter.clone(), -680f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(counter.clone(), td.num_samples + 1f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(counter.clone(), td.rate)]);

        test_aggregation(td);
    }

    #[test]
    fn aggregate_timer() {
        let mut td = TestData::new(1.);
        // timers should be properly aggregated into all agregates except value
        let mut timer = Metric::new(MetricValue::Timer(vec![1f64]), None, 1.);

        td.samples
            .iter()
            .map(|t| {
                let timer2 = Metric::new(MetricValue::Timer(vec![*t]), None, 1.);
                timer.accumulate(timer2).unwrap();
            })
        .last();

        td.expected.insert(Aggregate::Count, vec![(timer.clone(), 10f64)]);
        td.expected.insert(Aggregate::Min, vec![(timer.clone(), 1f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(timer.clone(), td.num_samples + 1f64)]);
        td.expected.insert(Aggregate::Max, vec![(timer.clone(), 84f64)]);
        td.expected.insert(Aggregate::Sum, vec![(timer.clone(), 316f64)]);
        // percentiles( 0.5 and 0.85 ) were counted in google spreadsheets
        td.expected.insert(Aggregate::Median, vec![(timer.clone(), 23.5f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(timer.clone(), td.rate)]);
        td.expected.insert(Aggregate::Percentile(0.85, 85), vec![(timer.clone(), 60.85f64)]);
        td.expected.insert(Aggregate::Mean, vec![(timer.clone(), 31.6f64)]);
        td.expected.insert(Aggregate::Last, vec![(timer.clone(), 64f64)]);

        td.to_aggregate.push(timer);

        test_aggregation(td);
    }

    #[test]
    fn aggregate_timer_sampled() {
        let mut td = TestData::new(0.1);
        // timers should be properly aggregated into all agregates except value
        let mut timer = Metric::new(MetricValue::Timer(vec![1f64]), None, 0.1);

        td.samples
            .iter()
            .map(|t| {
                let timer2 = Metric::new(MetricValue::Timer(vec![*t]), None, 0.1);
                timer.accumulate(timer2).unwrap();
            })
        .last();

        td.expected.insert(Aggregate::Count, vec![(timer.clone(), 100f64)]);
        td.expected.insert(Aggregate::Min, vec![(timer.clone(), 1f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(timer.clone(), td.num_samples + 1f64)]);
        td.expected.insert(Aggregate::Max, vec![(timer.clone(), 84f64)]);
        td.expected.insert(Aggregate::Sum, vec![(timer.clone(), 3160f64)]);
        // percentiles( 0.5 and 0.85 ) were counted in google spreadsheets
        td.expected.insert(Aggregate::Median, vec![(timer.clone(), 23.5f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(timer.clone(), td.rate)]);
        td.expected.insert(Aggregate::Percentile(0.85, 85), vec![(timer.clone(), 60.85f64)]);
        td.expected.insert(Aggregate::Mean, vec![(timer.clone(), 31.6f64)]);
        td.expected.insert(Aggregate::Last, vec![(timer.clone(), 64f64)]);

        td.to_aggregate.push(timer);

        test_aggregation(td);
    }
    #[test]
    fn aggregate_set() {
        let mut td = TestData::new(1.);
        // sets should be properly aggregated into count of uniques and update count
        let mut hs = HashSet::new();
        hs.insert(1f64.to_bits());
        let mut set = Metric::new(MetricValue::Set(hs), None, 1.);

        td.samples
            .iter()
            .map(|t| {
                let mut hs = HashSet::new();
                hs.insert((*t).to_bits());
                let set2 = Metric::new(MetricValue::Set(hs), None, 1.);
                set.accumulate(set2).unwrap();
            })
        .last();

        td.expected.insert(Aggregate::Count, vec![(set.clone(), 9f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(set.clone(), 10f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(set.clone(), td.rate)]);
        td.to_aggregate.push(set);
        test_aggregation(td);
    }
    #[test]
    fn aggregate_set_sampled() {
        let mut td = TestData::new(0.1);
        // sets should be properly aggregated into count of uniques and update count
        let mut hs = HashSet::new();
        hs.insert(1f64.to_bits());
        let mut set = Metric::new(MetricValue::Set(hs), None, 0.1);

        td.samples
            .iter()
            .map(|t| {
                let mut hs = HashSet::new();
                hs.insert((*t).to_bits());
                let set2 = Metric::new(MetricValue::Set(hs), None, 0.1);
                set.accumulate(set2).unwrap();
            })
        .last();

        td.expected.insert(Aggregate::Count, vec![(set.clone(), 90f64)]);
        td.expected.insert(Aggregate::UpdateCount, vec![(set.clone(), 10f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(set.clone(), td.rate)]);
        td.to_aggregate.push(set);
        test_aggregation(td);
    }

    #[test]
    fn aggregate_histogram() {
        let mut td = TestData::new(1.);
        // histograms should be properly aggregated into a number of buckets
        // as well as update_count and rate
        let smetric = StatsdMetric::new(-1f64, StatsdType::CustomHistogram(0f64, 100f64), None).unwrap();
        // for this request and 5 buckets, the buckets will be
        // 0, 25, 50, 75, 100

        let mut histogram = Metric::from_statsd(&smetric, td.buckets, None).unwrap();

        //let samples = vec![12f64, 43f64, 1f64, 9f64, 84f64, 55f64, 31f64, 16f64, 64f64];
        td.samples
            .iter()
            .map(|t| {
                let smetric = StatsdMetric::new(*t, StatsdType::CustomHistogram(0f64, 100f64), None).unwrap();
                histogram.accumulate_statsd(smetric).unwrap();
            })
        .last();

        td.expected.insert(Aggregate::Bucket(Some(0)), vec![(histogram.clone(), 1f64)]); // -inf..0
        td.expected.insert(Aggregate::Bucket(Some(1)), vec![(histogram.clone(), 4f64)]); // 0..25
        td.expected.insert(Aggregate::Bucket(Some(2)), vec![(histogram.clone(), 2f64)]); // 25..50
        td.expected.insert(Aggregate::Bucket(Some(3)), vec![(histogram.clone(), 2f64)]); // 50..75
        td.expected.insert(Aggregate::Bucket(Some(4)), vec![(histogram.clone(), 1f64)]); // 75..100
        td.expected.insert(Aggregate::Bucket(Some(5)), vec![(histogram.clone(), 0f64)]); // 100..inf
        td.expected.insert(Aggregate::UpdateCount, vec![(histogram.clone(), 10f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(histogram.clone(), td.rate)]);
        td.to_aggregate.push(histogram);

        test_aggregation(td);
    }

    #[test]
    fn aggregate_histogram_sampled() {
        let mut td = TestData::new(0.1);
        // histograms should be properly aggregated into a number of buckets
        // as well as update_count and rate
        let smetric = StatsdMetric::new(-1f64, StatsdType::CustomHistogram(0f64, 100f64), Some(0.1)).unwrap();
        // for this request and 5 buckets, the buckets will be
        // 0, 25, 50, 75, 100

        let mut histogram = Metric::from_statsd(&smetric, td.buckets, None).unwrap();

        //let samples = vec![12f64, 43f64, 1f64, 9f64, 84f64, 55f64, 31f64, 16f64, 64f64];
        td.samples
            .iter()
            .map(|t| {
                let smetric = StatsdMetric::new(*t, StatsdType::CustomHistogram(0f64, 100f64), Some(0.1)).unwrap();
                histogram.accumulate_statsd(smetric).unwrap();
            })
        .last();

        // because of sampling, all buckets must be ten times bigger
        td.expected.insert(Aggregate::Bucket(Some(0)), vec![(histogram.clone(), 10f64)]); // -inf..0
        td.expected.insert(Aggregate::Bucket(Some(1)), vec![(histogram.clone(), 40f64)]); // 0..25
        td.expected.insert(Aggregate::Bucket(Some(2)), vec![(histogram.clone(), 20f64)]); // 25..50
        td.expected.insert(Aggregate::Bucket(Some(3)), vec![(histogram.clone(), 20f64)]); // 50..75
        td.expected.insert(Aggregate::Bucket(Some(4)), vec![(histogram.clone(), 10f64)]); // 75..100
        td.expected.insert(Aggregate::Bucket(Some(5)), vec![(histogram.clone(), 0f64)]); // 100..inf
        td.expected.insert(Aggregate::UpdateCount, vec![(histogram.clone(), 10f64)]);
        td.expected.insert(td.rate_agg.clone(), vec![(histogram.clone(), td.rate)]);
        td.to_aggregate.push(histogram);

        test_aggregation(td);
    }
}
