# Build tide (debug)
build:
    cargo build

# Run clippy with warnings as errors
check:
    cargo clippy --all-targets -- -D warnings

# Build and install tide
install:
    cargo install --path .

# Run all tests
test:
    cargo test

# Run unit tests only
test-unit:
    cargo test --lib
