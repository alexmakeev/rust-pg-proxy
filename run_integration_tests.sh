#!/bin/bash

# Integration test runner for rust-pg-proxy
# Requires PostgreSQL to be running and accessible

set -e

# Check if required environment variables are set
if [ -z "$UPSTREAM_HOST" ]; then
    echo "Error: UPSTREAM_HOST environment variable is not set"
    echo ""
    echo "Required environment variables:"
    echo "  UPSTREAM_HOST       - PostgreSQL hostname (e.g., localhost)"
    echo "  UPSTREAM_PORT       - PostgreSQL port (default: 5432)"
    echo "  UPSTREAM_USER       - PostgreSQL username (default: postgres)"
    echo "  UPSTREAM_PASSWORD   - PostgreSQL password"
    echo "  UPSTREAM_DATABASE   - Database name (default: postgres)"
    echo ""
    echo "Example usage:"
    echo "  export UPSTREAM_HOST=localhost"
    echo "  export UPSTREAM_USER=postgres"
    echo "  export UPSTREAM_PASSWORD=mypassword"
    echo "  ./run_integration_tests.sh"
    exit 1
fi

echo "Running integration tests..."
echo "Upstream: $UPSTREAM_HOST:${UPSTREAM_PORT:-5432}/${UPSTREAM_DATABASE:-postgres}"
echo ""

# Run the ignored integration tests
cargo test --test integration_test -- --ignored --nocapture

echo ""
echo "Integration tests completed successfully!"
