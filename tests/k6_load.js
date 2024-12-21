import http from 'k6/http';
import { sleep, check } from 'k6';
import { Rate, Trend } from 'k6/metrics';
import { Counter } from 'k6/metrics';

// Custom metrics
const noveumLatencyTrend = new Trend('noveum_latency');
const groqLatencyTrend = new Trend('groq_latency');
const noveumSuccessRate = new Rate('noveum_success_rate');
const groqSuccessRate = new Rate('groq_success_rate');
const noveumFasterCounter = new Counter('noveum_faster');
const groqFasterCounter = new Counter('groq_faster');
const noveumRateLimitCounter = new Counter('noveum_rate_limits');
const groqRateLimitCounter = new Counter('groq_rate_limits');

// Configuration
const NOVEUM_URL = 'https://gate.noveum.ai/v1/chat/completions';
const GROQ_URL = 'https://api.groq.com/openai/v1/chat/completions';
const API_KEY = __ENV.API_KEY;
const MAX_RETRIES = 3;
const BASE_DELAY = 3; // Base delay between requests in seconds
const RATE_LIMIT_BACKOFF = 5; // Additional delay when rate limit is hit
const IS_CLOUD = __ENV.CLOUD === 'true'; // Control cloud execution via env variable

// Test payload
const PAYLOAD = {
  model: 'llama-3.1-8b-instant',
  messages: [
    {
      role: 'user',
      content: 'Write a poem'
    }
  ],
  stream: true,
  max_tokens: 300
};

// Helper function to make request with retries
function makeRequestWithRetry(url, payload, headers, serviceName) {
  let retries = 0;
  let lastResponse;
  let startTime;
  let latency;

  while (retries <= MAX_RETRIES) {
    startTime = Date.now();
    lastResponse = http.post(url, JSON.stringify(payload), {
      headers: headers,
      timeout: '60s',
    });
    latency = Date.now() - startTime;

    if (lastResponse.status === 429) {
      retries++;
      if (retries <= MAX_RETRIES) {
        const backoffTime = BASE_DELAY + (RATE_LIMIT_BACKOFF * retries);
        console.log(`${serviceName} rate limited, attempt ${retries}/${MAX_RETRIES}, backing off for ${backoffTime}s`);
        sleep(backoffTime);
        continue;
      }
    }
    break;
  }

  return { response: lastResponse, latency: latency };
}

// Base options for both local and cloud execution
const baseOptions = {
  vus: 1,
  iterations: 10,
  batchPerHost: 1,
  thresholds: {
    'noveum_success_rate': ['rate>0.95'],
    'groq_success_rate': ['rate>0.95'],
    'noveum_latency': ['p(95)<2000'],
    'groq_latency': ['p(95)<2000'],
  }
};

// Cloud-specific options
const cloudOptions = {
  ext: {
    loadimpact: {
      projectID: 3731397,
      name: `Noveum vs Groq Comparison - ${new Date().toISOString()}`,
      distribution: {
        'amazon:gb:london': { loadZone: 'amazon:gb:london', percent: 100 },
      },
    },
  },
};

// Export final options based on execution mode
export const options = IS_CLOUD 
  ? { ...baseOptions, ...cloudOptions }
  : baseOptions;

export default function() {
  // Headers for Noveum request
  const noveumHeaders = {
    'Authorization': `Bearer ${API_KEY}`,
    'Content-Type': 'application/json',
    'x-provider': 'groq'
  };

  // Headers for Groq request
  const groqHeaders = {
    'Authorization': `Bearer ${API_KEY}`,
    'Content-Type': 'application/json'
  };

  // Make Noveum request with retries
  const noveum = makeRequestWithRetry(NOVEUM_URL, PAYLOAD, noveumHeaders, 'Noveum');
  
  // Base delay between services
  sleep(BASE_DELAY);
  
  // Make Groq request with retries
  const groq = makeRequestWithRetry(GROQ_URL, PAYLOAD, groqHeaders, 'Groq');

  // Track rate limits
  if (noveum.response.status === 429) {
    noveumRateLimitCounter.add(1);
  }
  if (groq.response.status === 429) {
    groqRateLimitCounter.add(1);
  }

  // Record metrics for Noveum
  check(noveum.response, {
    'noveum status is 200': (r) => r.status === 200,
  });
  if (noveum.response.status === 200) {
    noveumLatencyTrend.add(noveum.latency);
  }
  noveumSuccessRate.add(noveum.response.status === 200);

  // Record metrics for Groq
  check(groq.response, {
    'groq status is 200': (r) => r.status === 200,
  });
  if (groq.response.status === 200) {
    groqLatencyTrend.add(groq.latency);
  }
  groqSuccessRate.add(groq.response.status === 200);

  // Compare which was faster (only for successful requests)
  if (noveum.response.status === 200 && groq.response.status === 200) {
    if (noveum.latency < groq.latency) {
      noveumFasterCounter.add(1);
    } else if (groq.latency < noveum.latency) {
      groqFasterCounter.add(1);
    }
  }

  // Log response details for debugging
  console.log(`
    Iteration ${__ITER}:
    Noveum: ${noveum.latency}ms (${noveum.response.status})${noveum.response.status === 429 ? ' - Rate Limited' : ''}
    Groq: ${groq.latency}ms (${groq.response.status})${groq.response.status === 429 ? ' - Rate Limited' : ''}
    Difference: ${Math.abs(noveum.latency - groq.latency)}ms
    Faster: ${noveum.latency < groq.latency ? 'Noveum' : 'Groq'}
  `);

  // Add longer delay between iterations
  sleep(BASE_DELAY);
}

export function handleSummary(data) {
  // Helper function to safely get metric values
  const getMetricValue = (metric, property, defaultValue = 0) => {
    return metric?.values?.[property] ?? defaultValue;
  };

  // Only show cloud URL and region info for cloud runs
  if (IS_CLOUD) {
    const cloudURL = `https://app.k6.io/runs/${__ENV.K6_CLOUD_RUN_ID}`;
    console.log('\n===========================================');
    console.log('🔍 View detailed results at:');
    console.log(cloudURL);
    console.log('\nTesting from region:');
    console.log('- Europe (London)');
    console.log('===========================================\n');
  }

  const noveumFaster = getMetricValue(data.metrics.noveum_faster, 'count');
  const groqFaster = getMetricValue(data.metrics.groq_faster, 'count');
  
  // Calculate additional statistics
  const noveumAvg = getMetricValue(data.metrics.noveum_latency, 'avg');
  const groqAvg = getMetricValue(data.metrics.groq_latency, 'avg');
  const avgDiff = Math.abs(noveumAvg - groqAvg);
  const avgDiffPercent = noveumAvg && groqAvg ? (avgDiff / Math.min(noveumAvg, groqAvg)) * 100 : 0;
  
  const noveumSuccessRate = getMetricValue(data.metrics.noveum_success_rate, 'rate', 1) * 100;
  const groqSuccessRate = getMetricValue(data.metrics.groq_success_rate, 'rate', 1) * 100;
  
  // Format numbers for better readability
  const formatMs = (num) => num ? `${num.toFixed(2)}ms` : 'N/A';
  const formatPercent = (num) => num ? `${num.toFixed(2)}%` : 'N/A';
  
  // Determine performance characteristics
  const fasterService = noveumAvg < groqAvg ? 'Noveum Gateway' : 'Direct Groq';
  const slowerService = noveumAvg < groqAvg ? 'Direct Groq' : 'Noveum Gateway';
  const consistencyWinner = 
    getMetricValue(data.metrics.noveum_latency, 'std') < getMetricValue(data.metrics.groq_latency, 'std')
      ? 'Noveum Gateway'
      : 'Direct Groq';
  
  // Add rate limit statistics
  const noveumRateLimits = getMetricValue(data.metrics.noveum_rate_limits, 'count');
  const groqRateLimits = getMetricValue(data.metrics.groq_rate_limits, 'count');
  
  return {
    'stdout': JSON.stringify({
      'Test Configuration': {
        'Total Iterations': options.iterations,
        'Virtual Users': options.vus,
        'Test Duration': `${(data.state.testRunDuration / 1000).toFixed(2)}s`,
        'Timestamp': new Date().toISOString()
      },
      'Noveum Gateway Performance': {
        'Latency Statistics': {
          'Average': formatMs(noveumAvg),
          'Median': formatMs(getMetricValue(data.metrics.noveum_latency, 'med')),
          'Min': formatMs(getMetricValue(data.metrics.noveum_latency, 'min')),
          'Max': formatMs(getMetricValue(data.metrics.noveum_latency, 'max')),
          '95th Percentile': formatMs(getMetricValue(data.metrics.noveum_latency, 'p(95)')),
          'Standard Deviation': formatMs(getMetricValue(data.metrics.noveum_latency, 'std'))
        },
        'Success Rate': formatPercent(noveumSuccessRate),
        'Times Faster': noveumFaster,
        'Win Rate': formatPercent((noveumFaster / (options.iterations || 1)) * 100),
        'Rate Limits': {
          'Count': noveumRateLimits,
          'Rate': formatPercent((noveumRateLimits / (options.iterations || 1)) * 100)
        }
      },
      'Direct Groq Performance': {
        'Latency Statistics': {
          'Average': formatMs(groqAvg),
          'Median': formatMs(getMetricValue(data.metrics.groq_latency, 'med')),
          'Min': formatMs(getMetricValue(data.metrics.groq_latency, 'min')),
          'Max': formatMs(getMetricValue(data.metrics.groq_latency, 'max')),
          '95th Percentile': formatMs(getMetricValue(data.metrics.groq_latency, 'p(95)')),
          'Standard Deviation': formatMs(getMetricValue(data.metrics.groq_latency, 'std'))
        },
        'Success Rate': formatPercent(groqSuccessRate),
        'Times Faster': groqFaster,
        'Win Rate': formatPercent((groqFaster / (options.iterations || 1)) * 100),
        'Rate Limits': {
          'Count': groqRateLimits,
          'Rate': formatPercent((groqRateLimits / (options.iterations || 1)) * 100)
        }
      },
      'Comparative Analysis': {
        'Speed Comparison': {
          'Faster Service': fasterService,
          'Average Difference': formatMs(avgDiff),
          'Difference Percentage': formatPercent(avgDiffPercent),
          'More Consistent Service': consistencyWinner
        },
        'Success Rate Comparison': {
          'Better Success Rate': noveumSuccessRate > groqSuccessRate ? 'Noveum Gateway' : 'Direct Groq',
          'Success Rate Difference': formatPercent(Math.abs(noveumSuccessRate - groqSuccessRate))
        },
        'Performance Summary': [
          `${fasterService} is faster by ${formatMs(avgDiff)} (${formatPercent(avgDiffPercent)} difference)`,
          `${consistencyWinner} shows more consistent performance`,
          `Success rates: Noveum ${formatPercent(noveumSuccessRate)} vs Groq ${formatPercent(groqSuccessRate)}`,
          `Win ratio: Noveum ${noveumFaster} vs Groq ${groqFaster} times`
        ],
        'Rate Limit Analysis': {
          'Service with Fewer Rate Limits': noveumRateLimits < groqRateLimits ? 'Noveum Gateway' : 'Direct Groq',
          'Rate Limit Difference': Math.abs(noveumRateLimits - groqRateLimits),
          'Noveum Rate Limits': noveumRateLimits,
          'Groq Rate Limits': groqRateLimits
        }
      },
      'Recommendations': [
        noveumSuccessRate < 95 ? '⚠️ Noveum Gateway success rate is below 95% threshold' : '✅ Noveum Gateway success rate is good',
        groqSuccessRate < 95 ? '⚠️ Direct Groq success rate is below 95% threshold' : '✅ Direct Groq success rate is good',
        avgDiffPercent > 20 ? `⚠️ Large performance gap (${formatPercent(avgDiffPercent)}) between services` : '✅ Performance difference is within acceptable range',
        getMetricValue(data.metrics.noveum_latency, 'p(95)') > 2000 ? '⚠️ Noveum Gateway 95th percentile latency exceeds 2s' : '✅ Noveum Gateway latency is within limits',
        getMetricValue(data.metrics.groq_latency, 'p(95)') > 2000 ? '⚠️ Direct Groq 95th percentile latency exceeds 2s' : '✅ Direct Groq latency is within limits',
        noveumRateLimits > 0 ? `⚠️ Noveum Gateway hit rate limits ${noveumRateLimits} times` : '✅ Noveum Gateway had no rate limits',
        groqRateLimits > 0 ? `⚠️ Direct Groq hit rate limits ${groqRateLimits} times` : '✅ Direct Groq had no rate limits',
      ]
    }, null, 2),
  };
}

/*
Run locally (from your machine's location):
k6 run --env API_KEY=your_key tests/k6_load.js

Run in cloud (from London region by default):
k6 run --env API_KEY=your_key --env CLOUD=true tests/k6_load.js

For multi-region cloud testing, modify the cloudOptions.ext.loadimpact.distribution object with desired regions:
Example:
{
  'amazon:gb:london': { loadZone: 'amazon:gb:london', percent: 34 },
  'amazon:us:ashburn': { loadZone: 'amazon:us:ashburn', percent: 33 },
  'amazon:ap:singapore': { loadZone: 'amazon:ap:singapore', percent: 33 }
}
*/