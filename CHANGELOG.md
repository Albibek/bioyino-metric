# 0.5.1
* implemented filtering NaNs out of Timer metrics

# 0.5.0
* added CustomHistogram type, allowing to parse and aggregate histogram with statically defined number of buckets and dynamic range, i.e. `some.metric:1|H1.2,1.8`
* removed DiffCounter metric type due to being broken and counter intuitive
* sampling was not considered at all before, now it is a first class citizen
* rate aggregation has been added as `update_counter / aggregation_period`, with period provided externally
* parser now parses floats from bytes without string conversion, giving around 10% speedup (see `benchmark-parser` branch for details)
* capnp protocol schema v2 has been added as a rework of metric encoding into more logical and error-prone structure

# 0.2.0

* aggregation API has changed, supporting custom percentiles, specifying a list of aggregates, etc
* metric name is now a separate structure allowing to process tag information
* metrics can now have tags in (yet only one) graphite format
* tags are placed in sorted order during aggregation and parsing so they are always correct to be used without creating separate structure for them
* aggregation modes are not supported to put aggregate names in tags or name postfixes
* name prefixing is available per aggregate
