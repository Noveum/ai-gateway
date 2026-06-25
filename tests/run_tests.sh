#!/bin/bash
# Script to run integration tests for the AI Gateway

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Print the header
echo -e "${BLUE}=========================================================${NC}"
echo -e "${BLUE}          MagicAPI Gateway Integration Tests             ${NC}"
echo -e "${BLUE}=========================================================${NC}"

# Check if .env.test exists, create from example if not
if [ ! -f ".env.test" ] && [ ! -f "tests/.env.test" ]; then
    echo -e "${YELLOW}No .env.test file found. Creating from example...${NC}"
    if [ -f "tests/.env.test.example" ]; then
        cp tests/.env.test.example .env.test
        echo -e "${GREEN}Created .env.test from tests/.env.test.example.${NC}"
        echo -e "${YELLOW}Please edit .env.test with your actual API keys before running the tests.${NC}"
        exit 1
    else
        echo -e "${RED}Error: Could not find tests/.env.test.example file.${NC}"
        exit 1
    fi
fi

# Check if gateway is running
echo -e "${BLUE}Checking if AI Gateway is running...${NC}"
if ! curl -s http://localhost:3000/health > /dev/null; then
    echo -e "${RED}Error: AI Gateway does not appear to be running. Please start it with:${NC}"
    echo -e "${YELLOW}cargo run${NC}"
    exit 1
fi

# If a specific provider is provided as an argument, only run tests for that provider
PROVIDER=$1
COMMAND="cargo test --test run_integration_tests"

if [ ! -z "$PROVIDER" ]; then
    echo -e "${BLUE}Running integration tests for ${GREEN}$PROVIDER${BLUE} provider...${NC}"
    COMMAND="$COMMAND $PROVIDER"
else
    echo -e "${BLUE}Running all integration tests...${NC}"
fi

# Add nocapture to see test output
COMMAND="$COMMAND -- --nocapture"

# Execute the command
echo -e "${YELLOW}Executing: ${COMMAND}${NC}"
eval $COMMAND

# Check exit status
if [ $? -eq 0 ]; then
    echo -e "${GREEN}✅ Integration tests completed successfully!${NC}"
else
    echo -e "${RED}❌ Some tests failed. Check the output above for details.${NC}"
    exit 1
fi 