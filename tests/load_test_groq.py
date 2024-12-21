#!/usr/bin/env python3
import asyncio
import aiohttp
import time
import statistics
import json
from typing import List, Dict, Tuple
import argparse
import logging
from datetime import datetime
import re
from collections import defaultdict

# Setup logging
logging.basicConfig(
    level=logging.DEBUG,
    format='%(asctime)s - %(levelname)s - %(message)s',
    handlers=[
        logging.StreamHandler(),
        logging.FileHandler(f'load_test_{datetime.now().strftime("%Y%m%d_%H%M%S")}.log')
    ]
)
logger = logging.getLogger(__name__)

NOVEUM_URL = "https://gate.noveum.ai/v1/chat/completions"
GROQ_URL = "https://api.groq.com/openai/v1/chat/completions"
GROQ_RPM_LIMIT = 10  # Groq's rate limit per minute

# Test payload
PAYLOAD = {
    "model": "llama-3.1-8b-instant",
    "messages": [
        {
            "role": "user",
            "content": "Write a poem"
        }
    ],
    "stream": True,
    "max_tokens": 300
}

class TestRound:
    def __init__(self):
        self.noveum_latencies: List[float] = []
        self.groq_latencies: List[float] = []
        self.start_time = None
        self.end_time = None
        
    @property
    def duration(self) -> float:
        return self.end_time - self.start_time if self.end_time and self.start_time else 0
    
    @property
    def noveum_avg(self) -> float:
        return statistics.mean(self.noveum_latencies) if self.noveum_latencies else 0
    
    @property
    def groq_avg(self) -> float:
        return statistics.mean(self.groq_latencies) if self.groq_latencies else 0
    
    @property
    def overhead(self) -> float:
        return self.noveum_avg - self.groq_avg

class RateLimiter:
    def __init__(self, rpm_limit: int):
        self.rpm_limit = rpm_limit
        self.requests = []
        self.lock = asyncio.Lock()
    
    async def acquire(self):
        async with self.lock:
            now = time.time()
            self.requests = [t for t in self.requests if now - t < 60]
            
            if len(self.requests) >= self.rpm_limit:
                wait_time = 60 - (now - self.requests[0])
                if wait_time > 0:
                    logger.debug(f"Rate limit reached, waiting for {wait_time:.2f}s")
                    await asyncio.sleep(wait_time)
            
            self.requests.append(now)

class LoadTester:
    def __init__(self, api_key: str, num_requests: int = 10, rounds: int = 3):
        self.api_key = api_key
        self.num_requests = num_requests
        self.rounds = rounds
        self.test_rounds: List[TestRound] = []
        self.request_counter = 0
        self.groq_rate_limiter = RateLimiter(GROQ_RPM_LIMIT)
        self.noveum_rate_limiter = RateLimiter(GROQ_RPM_LIMIT)

    async def make_request(self, session: aiohttp.ClientSession, url: str, headers: Dict, request_type: str, max_retries: int = 3) -> float:
        request_id = f"{request_type}_{self.request_counter}"
        self.request_counter += 1
        
        for retry in range(max_retries):
            if request_type == "groq":
                await self.groq_rate_limiter.acquire()
            else:
                await self.noveum_rate_limiter.acquire()
            
            logger.debug(f"Starting request {request_id} to {url} (attempt {retry + 1}/{max_retries})")
            start_time = time.time()
            
            try:
                async with session.post(url, json=PAYLOAD, headers=headers) as response:
                    logger.debug(f"Request {request_id} - Status: {response.status}")
                    
                    if response.status == 429:
                        error_text = await response.text()
                        error_data = json.loads(error_text)
                        retry_after = 5
                        if 'error' in error_data and 'message' in error_data['error']:
                            match = re.search(r'try again in (\d+\.?\d*)s', error_data['error']['message'])
                            if match:
                                retry_after = float(match.group(1))
                        logger.warning(f"Rate limit hit for {request_id}, waiting {retry_after}s before retry")
                        await asyncio.sleep(retry_after)
                        continue
                    
                    if response.status != 200:
                        error_text = await response.text()
                        logger.error(f"Request {request_id} failed with status {response.status}: {error_text}")
                        if retry < max_retries - 1:
                            await asyncio.sleep(1)
                            continue
                        return 0

                    chunks_received = 0
                    try:
                        async for _ in response.content:
                            chunks_received += 1
                    except Exception as e:
                        logger.error(f"Error reading stream for {request_id}: {str(e)}")
                        if retry < max_retries - 1:
                            await asyncio.sleep(1)
                            continue
                        return 0

                    duration = time.time() - start_time
                    logger.debug(f"Request {request_id} completed - Duration: {duration:.3f}s, Chunks: {chunks_received}")
                    return duration

            except Exception as e:
                logger.error(f"Error in request {request_id} to {url}: {str(e)}", exc_info=True)
                if retry < max_retries - 1:
                    await asyncio.sleep(1)
                    continue
                return 0
        
        return 0

    async def run_noveum_request(self, session: aiohttp.ClientSession, current_round: TestRound) -> None:
        headers = {
            "Authorization": f"Bearer {self.api_key}",
            "Content-Type": "application/json",
            "x-provider": "groq"
        }
        latency = await self.make_request(session, NOVEUM_URL, headers, "noveum")
        if latency > 0:
            current_round.noveum_latencies.append(latency)
            logger.info(f"Noveum request completed in {latency:.3f}s")

    async def run_groq_request(self, session: aiohttp.ClientSession, current_round: TestRound) -> None:
        headers = {
            "Authorization": f"Bearer {self.api_key}",
            "Content-Type": "application/json"
        }
        latency = await self.make_request(session, GROQ_URL, headers, "groq")
        if latency > 0:
            current_round.groq_latencies.append(latency)
            logger.info(f"Groq request completed in {latency:.3f}s")

    async def run_test_round(self, round_num: int) -> TestRound:
        logger.info(f"\nStarting test round {round_num}/{self.rounds}")
        current_round = TestRound()
        current_round.start_time = time.time()
        
        async with aiohttp.ClientSession() as session:
            for i in range(self.num_requests):
                logger.info(f"Round {round_num}, Request pair {i + 1}/{self.num_requests}")
                await asyncio.gather(
                    self.run_noveum_request(session, current_round),
                    self.run_groq_request(session, current_round)
                )
                if i < self.num_requests - 1:
                    await asyncio.sleep(1)
        
        current_round.end_time = time.time()
        return current_round

    async def run_load_test(self):
        logger.info(f"Starting load test with {self.num_requests} requests per endpoint, {self.rounds} rounds")
        overall_start = time.time()
        
        for round_num in range(1, self.rounds + 1):
            test_round = await self.run_test_round(round_num)
            self.test_rounds.append(test_round)
            if round_num < self.rounds:
                logger.info("Waiting 5 seconds between rounds...")
                await asyncio.sleep(5)
        
        total_time = time.time() - overall_start
        logger.info(f"All test rounds completed in {total_time:.2f} seconds")

    def print_results(self):
        logger.info("\n=== Load Test Results ===")
        
        # Collect all latencies across rounds
        all_noveum_latencies = []
        all_groq_latencies = []
        round_overheads = []
        
        for round_num, test_round in enumerate(self.test_rounds, 1):
            logger.info(f"\nRound {round_num} Results:")
            logger.info(f"Duration: {test_round.duration:.2f}s")
            logger.info(f"Noveum Average: {test_round.noveum_avg:.3f}s")
            logger.info(f"Groq Average: {test_round.groq_avg:.3f}s")
            logger.info(f"Overhead: {test_round.overhead:.3f}s")
            
            all_noveum_latencies.extend(test_round.noveum_latencies)
            all_groq_latencies.extend(test_round.groq_latencies)
            round_overheads.append(test_round.overhead)
        
        logger.info("\n=== Statistical Analysis ===")
        
        def print_service_stats(name: str, latencies: List[float]):
            logger.info(f"\n{name} Overall Stats:")
            logger.info(f"Total Requests: {len(latencies)}")
            logger.info(f"Average Latency: {statistics.mean(latencies):.3f}s")
            logger.info(f"Median Latency: {statistics.median(latencies):.3f}s")
            logger.info(f"Min Latency: {min(latencies):.3f}s")
            logger.info(f"Max Latency: {max(latencies):.3f}s")
            logger.info(f"95th Percentile: {sorted(latencies)[int(len(latencies)*0.95)]:.3f}s")
            logger.info(f"Standard Deviation: {statistics.stdev(latencies):.3f}s")
        
        print_service_stats("Noveum Gateway", all_noveum_latencies)
        print_service_stats("Direct Groq", all_groq_latencies)
        
        # Overall analysis
        avg_overhead = statistics.mean(round_overheads)
        overhead_stdev = statistics.stdev(round_overheads)
        
        logger.info("\n=== Final Analysis ===")
        logger.info(f"Average Overhead across all rounds: {avg_overhead:.3f}s ± {overhead_stdev:.3f}s")
        
        # Determine which service is faster
        faster_count = defaultdict(int)
        for test_round in self.test_rounds:
            if test_round.overhead > 0:
                faster_count["groq"] += 1
            elif test_round.overhead < 0:
                faster_count["noveum"] += 1
            else:
                faster_count["tie"] += 1
        
        logger.info("\nRound-by-round winner analysis:")
        logger.info(f"Noveum faster: {faster_count['noveum']}/{self.rounds} rounds")
        logger.info(f"Groq faster: {faster_count['groq']}/{self.rounds} rounds")
        logger.info(f"Ties: {faster_count['tie']}/{self.rounds} rounds")
        
        # Statistical significance
        if abs(avg_overhead) > overhead_stdev:
            if avg_overhead > 0:
                logger.info("\nGroq is consistently faster (difference > standard deviation)")
            else:
                logger.info("\nNoveum is consistently faster (difference > standard deviation)")
        else:
            logger.info("\nNo statistically significant difference in speed (difference < standard deviation)")

def main():
    parser = argparse.ArgumentParser(description='Run load test comparing Noveum Gateway vs Direct Groq API')
    parser.add_argument('--api-key', required=True, help='Groq API Key')
    parser.add_argument('--requests', type=int, default=10, help='Number of requests per round (default: 10)')
    parser.add_argument('--rounds', type=int, default=3, help='Number of test rounds (default: 3)')
    parser.add_argument('--log-level', choices=['DEBUG', 'INFO', 'WARNING', 'ERROR'], default='INFO',
                      help='Set the logging level (default: INFO)')
    args = parser.parse_args()

    logging.getLogger().setLevel(getattr(logging, args.log_level))
    
    logger.info(f"Starting load test with configuration:")
    logger.info(f"Requests per round: {args.requests}")
    logger.info(f"Number of rounds: {args.rounds}")
    logger.info(f"Log level: {args.log_level}")

    tester = LoadTester(args.api_key, args.requests, args.rounds)
    asyncio.run(tester.run_load_test())
    tester.print_results()

if __name__ == "__main__":
    main()

# python tests/load_test_groq.py --api-key YOUR_KEY --requests 10 --rounds 3