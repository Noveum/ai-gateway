# Load Testing Documentation

This directory contains two load testing scripts specifically designed to compare performance between Noveum AI Gateway and direct Groq API calls. Currently, these tests are focused only on Groq integration:
- `load_test_groq.py`: Python-based load testing script for Groq performance comparison
- `k6_load.js`: k6-based load testing script for Groq performance comparison with cloud execution support

> **Note**: These load tests are currently implemented only for Groq API comparisons. Support for other providers will be added in future updates.

## Prerequisites

### For Python Load Test
```bash
pip install aiohttp asyncio python-dotenv
```

### For k6 Load Test
1. Install k6:
   ```bash
   # macOS
   brew install k6

   # Windows
   winget install k6

   # Linux
   sudo apt-get install k6
   ```
2. (Optional) Sign up for k6 Cloud for cloud execution capabilities

## Running the Tests

### Python Load Test (`load_test_groq.py`)

1. Set up environment variables (Groq API Key):
   ```bash
   export API_KEY=your_groq_api_key
   ```
   Or create a `.env` file:
   ```
   API_KEY=your_groq_api_key
   ```

2. Run the Groq comparison test:
   ```bash
   python load_test_groq.py
   ```

#### Configuration Options
- Default: 3 rounds with 10 requests each to both Noveum (using Groq) and direct Groq API
- Modify the script constants to adjust:
  - `NUM_ROUNDS`: Number of test rounds
  - `REQUESTS_PER_ROUND`: Requests per round
  - `DELAY_BETWEEN_REQUESTS`: Delay between requests (seconds)

#### Example Output
```
=== Load Test Results ===
Round 1 Results:
Duration: 63.33s
Noveum Average: 0.741s
Groq Average: 0.800s
Overhead: -0.059s

...

=== Statistical Analysis ===
Noveum Gateway Overall Stats:
Total Requests: 30
Average Latency: 0.754s
Median Latency: 0.760s
...
```

### k6 Load Test (`k6_load.js`)

#### Local Execution
Run the Groq comparison test locally:
```bash
k6 run --env API_KEY=your_groq_api_key tests/k6_load.js
```

#### Cloud Execution
Run the Groq comparison test in k6 Cloud:
```bash
k6 run --env API_KEY=your_groq_api_key --env CLOUD=true tests/k6_load.js
```

#### Configuration Options
Modify the script constants:
- `options.vus`: Number of virtual users
- `options.iterations`: Total number of iterations
- `BASE_DELAY`: Delay between requests
- `MAX_RETRIES`: Maximum retry attempts for rate-limited requests

For cloud execution, you can modify the distribution of load zones in `cloudOptions.ext.loadimpact.distribution`.

#### Example Output
```
=== Test Results ===
Noveum Gateway Performance:
- Average Latency: 908.1ms
- Success Rate: 100%
- 95th Percentile: 1333.25ms

Direct Groq Performance:
- Average Latency: 672.2ms
- Success Rate: 100%
- 95th Percentile: 740.9ms
```

## Interpreting Results

The tests provide several key metrics:
1. **Latency Comparison**: Average response times for both services
2. **Success Rates**: Percentage of successful requests
3. **Statistical Analysis**: Including median, min/max, and 95th percentile
4. **Rate Limit Handling**: Count of rate limit encounters and recovery
5. **Performance Overhead**: Additional latency introduced by the gateway

## Best Practices

1. **Rate Limits**
   - Be mindful of Groq's rate limits
   - Use appropriate delays between requests
   - Monitor rate limit errors in logs

2. **Test Duration**
   - Run multiple rounds for consistent results
   - Allow sufficient time between tests
   - Consider time-of-day effects

3. **Data Analysis**
   - Look at both average and percentile metrics
   - Consider statistical significance
   - Account for network variability

## Troubleshooting

### Common Issues

1. Rate Limit Errors
   ```
   Solution: Increase BASE_DELAY or reduce concurrent requests
   ```

2. Connection Timeouts
   ```
   Solution: Check network connectivity and API key validity
   ```

3. Inconsistent Results
   ```
   Solution: Run multiple test rounds and look at aggregate statistics
   ```

## Contributing

Feel free to improve these tests by:
1. Adding support for other providers (e.g., Anthropic, OpenAI)
2. Improving error handling
3. Enhancing reporting
4. Adding cross-provider comparison capabilities

Submit a PR with your improvements! 