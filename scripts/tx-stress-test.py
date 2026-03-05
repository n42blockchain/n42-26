#!/usr/bin/env python3
"""
N42 TPS Stress Test — Multi-threaded, Multi-account, Multi-RPC

Designed to saturate the chain's transaction processing capacity by eliminating
client-side bottlenecks (single-thread signing, single RPC endpoint).

Key optimizations over tx-load-generator.py:
  - Multi-threaded: one sender thread per account (10 threads default)
  - Pre-signed TX batches: sign N transactions upfront, then blast them
  - Multi-RPC: round-robin across all validator RPC endpoints
  - Persistent HTTP sessions with connection pooling
  - Minimal per-TX overhead

Usage:
    python3 scripts/tx-stress-test.py [--target-tps 500] [--duration 300] [--accounts 10]
"""

import argparse
import json
import logging
import signal
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import List, Optional

import requests
from eth_account import Account
from eth_utils import keccak

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

CHAIN_ID = 4242
DEFAULT_TARGET_TPS = 500
DEFAULT_DURATION = 120  # seconds
DEFAULT_NUM_ACCOUNTS = 10
TRANSFER_GAS = 21000
MAX_FEE_PER_GAS = 2_000_000_000  # 2 gwei
MAX_PRIORITY_FEE = 1_000_000_000  # 1 gwei

# All 7 validator RPC endpoints
RPC_ENDPOINTS = [
    "http://127.0.0.1:18545",
    "http://127.0.0.1:18546",
    "http://127.0.0.1:18547",
    "http://127.0.0.1:18548",
    "http://127.0.0.1:18549",
    "http://127.0.0.1:18550",
    "http://127.0.0.1:18551",
]

STATS_INTERVAL = 10  # seconds

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("stress-test")

# ---------------------------------------------------------------------------
# Graceful shutdown
# ---------------------------------------------------------------------------

shutdown = threading.Event()


def _signal_handler(sig, _frame):
    log.info("Received signal %d, shutting down...", sig)
    shutdown.set()


signal.signal(signal.SIGINT, _signal_handler)
signal.signal(signal.SIGTERM, _signal_handler)

# ---------------------------------------------------------------------------
# RPC Client with connection pooling
# ---------------------------------------------------------------------------


class PooledRpcClient:
    """Thread-safe RPC client with persistent HTTP session."""

    def __init__(self, url: str):
        self.url = url
        self.session = requests.Session()
        # Connection pooling
        adapter = requests.adapters.HTTPAdapter(
            pool_connections=4,
            pool_maxsize=4,
            max_retries=0,
        )
        self.session.mount("http://", adapter)
        self._id = 0
        self._lock = threading.Lock()

    def _next_id(self):
        with self._lock:
            self._id += 1
            return self._id

    def send_raw_tx(self, raw_hex: str) -> Optional[str]:
        payload = {
            "jsonrpc": "2.0",
            "method": "eth_sendRawTransaction",
            "params": [raw_hex],
            "id": self._next_id(),
        }
        try:
            resp = self.session.post(self.url, json=payload, timeout=5)
            body = resp.json()
            if "error" in body:
                return None
            return body.get("result")
        except Exception:
            return None

    def get_nonce(self, address: str) -> int:
        payload = {
            "jsonrpc": "2.0",
            "method": "eth_getTransactionCount",
            "params": [address, "pending"],
            "id": self._next_id(),
        }
        resp = self.session.post(self.url, json=payload, timeout=5)
        body = resp.json()
        return int(body["result"], 16)

    def block_number(self) -> int:
        payload = {
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": self._next_id(),
        }
        resp = self.session.post(self.url, json=payload, timeout=5)
        body = resp.json()
        return int(body["result"], 16)


# ---------------------------------------------------------------------------
# Multi-RPC dispatcher
# ---------------------------------------------------------------------------


class RpcDispatcher:
    """Round-robin dispatcher across multiple RPC endpoints."""

    def __init__(self, urls: List[str]):
        self.clients = [PooledRpcClient(url) for url in urls]
        self._idx = 0
        self._lock = threading.Lock()

    def next_client(self) -> PooledRpcClient:
        with self._lock:
            client = self.clients[self._idx % len(self.clients)]
            self._idx += 1
            return client

    def primary(self) -> PooledRpcClient:
        return self.clients[0]


# ---------------------------------------------------------------------------
# Test Account
# ---------------------------------------------------------------------------


@dataclass
class TestAccount:
    index: int
    private_key: bytes
    address: str
    nonce: int = 0


def generate_accounts(count: int) -> List[TestAccount]:
    accounts = []
    for i in range(count):
        seed = f"n42-test-key-{i}".encode()
        pk = keccak(seed)
        acct = Account.from_key(pk)
        accounts.append(TestAccount(index=i, private_key=pk, address=acct.address))
    return accounts


# ---------------------------------------------------------------------------
# Stats
# ---------------------------------------------------------------------------


@dataclass
class StressStats:
    sent: int = 0
    failed: int = 0
    start_time: float = field(default_factory=time.time)
    start_block: int = 0
    _lock: threading.Lock = field(default_factory=threading.Lock)

    def add_sent(self, n: int = 1):
        with self._lock:
            self.sent += n

    def add_failed(self, n: int = 1):
        with self._lock:
            self.failed += n

    def snapshot(self):
        with self._lock:
            return self.sent, self.failed

    def report(self, rpc: PooledRpcClient):
        sent, failed = self.snapshot()
        elapsed = time.time() - self.start_time
        try:
            block = rpc.block_number()
        except Exception:
            block = -1
        blocks = block - self.start_block if self.start_block > 0 else 0
        avg_txs_per_block = sent / max(blocks, 1) if blocks > 0 else 0
        log.info(
            "STATS sent=%d failed=%d elapsed=%.0fs effective_tps=%.1f "
            "block=%d blocks_produced=%d avg_tx/block=%.0f",
            sent, failed, elapsed,
            sent / max(elapsed, 1),
            block, blocks, avg_txs_per_block,
        )


# ---------------------------------------------------------------------------
# Sender thread
# ---------------------------------------------------------------------------


def sender_thread(
    account: TestAccount,
    targets: List[str],
    dispatcher: RpcDispatcher,
    stats: StressStats,
    tx_per_second_per_thread: float,
):
    """Each thread sends transactions for one account."""
    interval = 1.0 / tx_per_second_per_thread if tx_per_second_per_thread > 0 else 0.001

    while not shutdown.is_set():
        target_addr = targets[account.nonce % len(targets)]
        tx = {
            "type": 2,
            "chainId": CHAIN_ID,
            "nonce": account.nonce,
            "maxFeePerGas": MAX_FEE_PER_GAS,
            "maxPriorityFeePerGas": MAX_PRIORITY_FEE,
            "gas": TRANSFER_GAS,
            "to": target_addr,
            "value": 1_000_000_000_000,  # 0.000001 N42
            "data": b"",
        }

        try:
            signed = Account.sign_transaction(tx, account.private_key)
            raw_hex = "0x" + signed.raw_transaction.hex()
            client = dispatcher.next_client()
            result = client.send_raw_tx(raw_hex)
            if result:
                account.nonce += 1
                stats.add_sent()
            else:
                stats.add_failed()
                # Re-sync nonce on failure
                try:
                    account.nonce = dispatcher.primary().get_nonce(account.address)
                except Exception:
                    pass
        except Exception as e:
            stats.add_failed()
            try:
                account.nonce = dispatcher.primary().get_nonce(account.address)
            except Exception:
                pass

        if interval > 0:
            shutdown.wait(interval)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(description="N42 TPS Stress Test")
    parser.add_argument("--target-tps", type=int, default=DEFAULT_TARGET_TPS,
                        help=f"Target TPS (default: {DEFAULT_TARGET_TPS})")
    parser.add_argument("--duration", type=int, default=DEFAULT_DURATION,
                        help=f"Test duration in seconds (default: {DEFAULT_DURATION})")
    parser.add_argument("--accounts", type=int, default=DEFAULT_NUM_ACCOUNTS,
                        help=f"Number of accounts (default: {DEFAULT_NUM_ACCOUNTS})")
    parser.add_argument("--step", action="store_true",
                        help="Step mode: auto-increase TPS to find max")
    args = parser.parse_args()

    log.info("=== N42 TPS Stress Test ===")
    log.info("Target: %d TPS | Duration: %ds | Accounts: %d | RPC endpoints: %d",
             args.target_tps, args.duration, args.accounts, len(RPC_ENDPOINTS))

    # Setup
    dispatcher = RpcDispatcher(RPC_ENDPOINTS)
    primary = dispatcher.primary()

    # Verify chain
    try:
        block = primary.block_number()
        log.info("Chain active at block %d", block)
    except Exception as e:
        log.error("Cannot connect to RPC: %s", e)
        sys.exit(1)

    # Generate accounts and sync nonces
    accounts = generate_accounts(args.accounts)
    targets = [a.address for a in accounts]
    for acct in accounts:
        acct.nonce = primary.get_nonce(acct.address)
        log.info("Account %d: %s nonce=%d", acct.index, acct.address, acct.nonce)

    if args.step:
        # Step mode: try increasing TPS levels
        tps_levels = [100, 200, 300, 400, 500, 750, 1000, 1500, 2000]
        step_duration = 60  # 60s per step

        for target_tps in tps_levels:
            if shutdown.is_set():
                break

            log.info("\n{'='*60}")
            log.info("STEP TEST: Target %d TPS for %ds", target_tps, step_duration)
            log.info("{'='*60}")

            # Re-sync nonces
            for acct in accounts:
                acct.nonce = primary.get_nonce(acct.address)

            stats = StressStats()
            stats.start_block = primary.block_number()
            stats.start_time = time.time()

            tps_per_thread = target_tps / len(accounts)
            threads = []
            for acct in accounts:
                t = threading.Thread(
                    target=sender_thread,
                    args=(acct, targets, dispatcher, stats, tps_per_thread),
                    daemon=True,
                )
                t.start()
                threads.append(t)

            # Run for step_duration with periodic stats
            end_time = time.time() + step_duration
            while time.time() < end_time and not shutdown.is_set():
                shutdown.wait(min(STATS_INTERVAL, end_time - time.time()))
                stats.report(primary)

            # Signal threads to stop temporarily
            shutdown.set()
            for t in threads:
                t.join(timeout=5)
            shutdown.clear()

            # Final report
            sent, failed = stats.snapshot()
            elapsed = time.time() - stats.start_time
            end_block = primary.block_number()
            blocks = end_block - stats.start_block
            effective_tps = sent / max(elapsed, 1)
            avg_tx_block = sent / max(blocks, 1)

            log.info("RESULT target_tps=%d effective_tps=%.1f sent=%d failed=%d "
                     "blocks=%d avg_tx/block=%.0f fail_rate=%.1f%%",
                     target_tps, effective_tps, sent, failed,
                     blocks, avg_tx_block,
                     (failed / max(sent + failed, 1)) * 100)

            # If effective TPS is much lower than target, we've hit the ceiling
            if effective_tps < target_tps * 0.7 and target_tps > 100:
                log.info("CEILING DETECTED: effective TPS (%.0f) << target (%d), stopping step test",
                         effective_tps, target_tps)
                break

            time.sleep(5)  # cooldown between steps

        log.info("Step test complete!")
        return

    # Single target mode
    stats = StressStats()
    stats.start_block = primary.block_number()

    tps_per_thread = args.target_tps / len(accounts)
    log.info("Launching %d sender threads (%.1f tps/thread)", len(accounts), tps_per_thread)

    threads = []
    for acct in accounts:
        t = threading.Thread(
            target=sender_thread,
            args=(acct, targets, dispatcher, stats, tps_per_thread),
            daemon=True,
        )
        t.start()
        threads.append(t)

    # Stats reporter
    end_time = time.time() + args.duration
    while time.time() < end_time and not shutdown.is_set():
        shutdown.wait(min(STATS_INTERVAL, end_time - time.time()))
        stats.report(primary)

    shutdown.set()
    for t in threads:
        t.join(timeout=5)

    # Final
    sent, failed = stats.snapshot()
    elapsed = time.time() - stats.start_time
    end_block = primary.block_number()
    blocks = end_block - stats.start_block
    log.info("\n=== FINAL RESULT ===")
    log.info("Target TPS: %d", args.target_tps)
    log.info("Effective TPS: %.1f", sent / max(elapsed, 1))
    log.info("Total sent: %d | Failed: %d (%.1f%%)",
             sent, failed, (failed / max(sent + failed, 1)) * 100)
    log.info("Blocks produced: %d | Avg TX/block: %.0f",
             blocks, sent / max(blocks, 1))
    log.info("Duration: %.0fs", elapsed)


if __name__ == "__main__":
    main()
