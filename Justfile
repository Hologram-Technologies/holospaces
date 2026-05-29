# holospaces

# Run the full V&V (evaluate holospaces against external authoritative standards;
# defined in docs/ arc42 chapter 10, implemented in vv/).
vv:
    vv/run.sh

# Build + validate the documentation (the specification-conformance suite, V1–V8).
docs:
    docs/scripts/build.sh
