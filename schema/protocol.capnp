@0xd87a49a1c493df22;

# This schema defines a way to deliver metrics in both ways:
# as pre-aggregated shapshots and as new metrics
# Please note, that capnproto allows to skip sending any fields
# if they are separate types, so there is almost no need to integrate
# option-like type into schema type system.
# Bio will try to accept unspecified fields with some defaults,
# but may fail if it cannot get ones it needs

# A message type for using in network interactions when metrics are involved
# the difference between snapshot and multi is that snapshot will be sent
# only to backend not to other nodes in the network
struct Message {
    union {
        single @0 :Metric;
        multi @1 :List(Metric);
        snapshot @2 :List(Metric);
    }
}

struct Metric {

    # everyone should have a name, even metrics
    name @0 :Text;

    # each metric has a value when it's sent
    value @1 :Float64;

    # some types also imply additional internal values depending on metric type
    type @2 :MetricType;

    # a timesamp can optionally be sent, i.e. for historic reasons
    timestamp @3 :Timestamp;

    # additional useful data about metric
    meta @4 :MetricMeta;
}

struct Timestamp {
    ts @0 :UInt64;
}

struct MetricType {
    union {
        # counter value is stored inside it's value
        counter @0 :Void;

        # for diff counter the metric value stores current counter value
        # the internal value stores last received counter change for differentiating
        diffCounter @1 :Float64;

        # timer holds all values for further stats counting
        timer @2 :List(Float64);

        # gauge can work as a counter too when `+value` or `-value` is received
        gauge @3 :Gauge;

        # set holds all values for further cardinality estimation
        set @4 :List(UInt64);

        # we count buckets using "right of or equal" rule
        # example: 10 buckets in 0-10 range will look like
        # c, (0, c0), (1, c1), ... (10, c10)
        # where c0 will store number of values right of zero, including zero itself
        # and left of 1 NOT including 1 itself
        # the last bucket - c10 is catch-all bucket for all values >= 10
        # and the first value c is the catch-all for all values < 0
        customHistogram @5 :CustomHistogram;
    }
}

struct Gauge {
    union {
        unsigned @0 :Void;
        signed @1 :Int8;
    }
}

struct MetricMeta {
    sampling @0 :Sampling;
    updateCounter @1 :UInt32;
#    tags @2 :List(Tag);
}

struct Sampling  {
    sampling @0 :Float32;
}

struct Tag {
    key @0 :Text;
    value @1 :Text;
}

struct CustomHistogram {
    leftBucket @0 :UInt64;
    buckets @1 :List(RightOf);
}

struct RightOf {
    value @0 :Float64;
    counter @1 :UInt64;
}
