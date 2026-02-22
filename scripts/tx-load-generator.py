#!/usr/bin/env python3
"""
N42 Testnet Transaction Load Generator

Continuously generates mixed transactions (native N42 transfers + ERC-20 calls)
for the 7-node testnet. Designed for multi-day unattended operation.

Two-phase operation:
  Phase 1: Deploy TestUSDT ERC-20 contract
  Phase 2: Continuous mixed transaction generation (70% transfers, 30% ERC-20)

Key reliability features:
  - Three-layer nonce management (local counter + error rollback + periodic sync)
  - Exponential backoff on consecutive errors
  - Balance verification at startup
  - Graceful shutdown on SIGINT/SIGTERM

CRITICAL: Uses eth_utils.keccak (raw Keccak-256), NOT hashlib.sha3_256 (NIST SHA3-256).
         Different padding produces different hashes — only Keccak-256 matches Rust's
         alloy_primitives::keccak256 used in genesis.rs for test key derivation.

Usage:
    python3 scripts/tx-load-generator.py [--rpc URL] [--rate TPS] [--no-erc20]
"""

import argparse
import json
import logging
import os
import random
import signal
import sys
import threading
import time
from dataclasses import dataclass, field
from typing import List, Optional

import requests
from eth_account import Account

# CRITICAL: Use eth_utils.keccak (Keccak-256), NOT hashlib.sha3_256 (NIST SHA3-256).
# Python's hashlib.sha3_256 uses NIST SHA3 padding (0x06) while Ethereum uses
# original Keccak padding (0x01). They produce DIFFERENT outputs for the same input.
from eth_utils import keccak, to_checksum_address

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

CHAIN_ID = 4242
NUM_ACCOUNTS = 10
DEFAULT_RPC = "http://127.0.0.1:18545"
DEFAULT_RATE = 2  # tx per second

# Gas limits
TRANSFER_GAS = 21000
ERC20_TRANSFER_GAS = 65000
DEPLOY_GAS = 1_500_000

# Gas prices (EIP-1559)
MAX_FEE_PER_GAS = 2_000_000_000       # 2 gwei
MAX_PRIORITY_FEE = 1_000_000_000      # 1 gwei

# ERC-20 mix: one transfer every N TXs (≈ every 2 blocks at 2tx/s, 4s slot)
ERC20_TX_INTERVAL = 16

# Reliability parameters
STATS_INTERVAL = 30          # seconds between stats reports
NONCE_SYNC_INTERVAL = 300    # 5 minutes between full nonce syncs
MAX_CONSECUTIVE_ERRORS = 20  # trigger backoff after this many
BACKOFF_MIN = 5              # minimum backoff seconds
BACKOFF_MAX = 30             # maximum backoff seconds

# ERC-20 transfer(address,uint256) function selector
ERC20_TRANSFER_SELECTOR = bytes.fromhex("a9059cbb")


def _encode_erc20_transfer(to_address: str, amount: int) -> bytes:
    """ABI-encode transfer(address,uint256) calldata."""
    to_bytes = bytes.fromhex(to_address[2:])
    return (
        ERC20_TRANSFER_SELECTOR +
        b"\x00" * 12 + to_bytes +
        amount.to_bytes(32, "big")
    )

# Resolve project root from script location
_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
_PROJECT_DIR = os.path.dirname(_SCRIPT_DIR)
_TESTUSDT_HEX_PATH = os.path.join(_PROJECT_DIR, "tests", "e2e", "contracts", "TestUSDT.hex")


def _load_testusdt_bytecode(hex_path: Optional[str] = None) -> str:
    """Load TestUSDT deployment bytecode from hex file.

    Reads from tests/e2e/contracts/TestUSDT.hex to avoid embedding errors
    in long hex strings (the bytecode is 1700+ hex chars).
    """
    path = hex_path or _TESTUSDT_HEX_PATH
    if not os.path.exists(path):
        log.error("TestUSDT.hex not found at %s", path)
        log.error("Run from the project root or pass --contract-hex")
        sys.exit(1)
    with open(path, "r") as f:
        bytecode = f.read().strip()
    # Validate: must be valid hex
    try:
        bytes.fromhex(bytecode)
    except ValueError as e:
        log.error("Invalid hex in %s: %s", path, e)
        sys.exit(1)
    return bytecode

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("tx-generator")

# ---------------------------------------------------------------------------
# Graceful shutdown
# ---------------------------------------------------------------------------

shutdown_event = threading.Event()


def _signal_handler(sig, _frame):
    log.info("Received signal %d, shutting down gracefully...", sig)
    shutdown_event.set()


signal.signal(signal.SIGINT, _signal_handler)
signal.signal(signal.SIGTERM, _signal_handler)

# ---------------------------------------------------------------------------
# JSON-RPC Client
# ---------------------------------------------------------------------------


class RpcClient:
    """Minimal JSON-RPC client for N42 node interaction."""

    def __init__(self, url: str):
        self.url = url
        self.session = requests.Session()
        self._next_id = 0

    def _call(self, method: str, params: list = None):
        self._next_id += 1
        payload = {
            "jsonrpc": "2.0",
            "method": method,
            "params": params or [],
            "id": self._next_id,
        }
        resp = self.session.post(self.url, json=payload, timeout=10)
        resp.raise_for_status()
        body = resp.json()
        if "error" in body:
            raise RuntimeError(f"RPC {method}: {body['error']}")
        return body.get("result")

    def block_number(self) -> int:
        return int(self._call("eth_blockNumber"), 16)

    def get_balance(self, address: str) -> int:
        return int(self._call("eth_getBalance", [address, "latest"]), 16)

    def get_nonce(self, address: str) -> int:
        return int(self._call("eth_getTransactionCount", [address, "pending"]), 16)

    def send_raw_transaction(self, raw_tx_hex: str) -> str:
        return self._call("eth_sendRawTransaction", [raw_tx_hex])

    def get_receipt(self, tx_hash: str) -> Optional[dict]:
        return self._call("eth_getTransactionReceipt", [tx_hash])

    def chain_id(self) -> int:
        return int(self._call("eth_chainId"), 16)


# ---------------------------------------------------------------------------
# Account Manager
# ---------------------------------------------------------------------------


@dataclass
class ManagedAccount:
    """A test account with local nonce tracking."""
    private_key: bytes
    address: str
    nonce: int = 0


class AccountManager:
    """Generates and manages deterministic test accounts with nonce tracking.

    Key derivation matches tests/e2e/src/genesis.rs:
        private_key = keccak256(b"n42-test-key-{i}")
    """

    def __init__(self):
        self.accounts: List[ManagedAccount] = []
        self._generate()

    def _generate(self):
        for i in range(NUM_ACCOUNTS):
            seed = f"n42-test-key-{i}".encode()
            # CRITICAL: Use Keccak-256 (eth_utils.keccak), NOT NIST SHA3-256!
            private_key = keccak(seed)
            acct = Account.from_key(private_key)
            self.accounts.append(ManagedAccount(
                private_key=private_key,
                address=acct.address,
            ))
        log.info("Generated %d test accounts", len(self.accounts))

    # -- Layer 3: Periodic full sync ------------------------------------------

    def sync_all_nonces(self, rpc: RpcClient):
        """Sync nonces for all accounts from chain state."""
        for acct in self.accounts:
            acct.nonce = rpc.get_nonce(acct.address)
        log.info("Full nonce sync complete")

    # -- Layer 2: Immediate error-recovery sync --------------------------------

    def sync_nonce(self, index: int, rpc: RpcClient):
        """Re-sync a single account nonce from chain after TX error."""
        acct = self.accounts[index]
        acct.nonce = rpc.get_nonce(acct.address)

    # -- Layer 1: Local counter ------------------------------------------------

    def get_and_increment(self, index: int) -> int:
        """Return current nonce and bump the local counter."""
        acct = self.accounts[index]
        n = acct.nonce
        acct.nonce += 1
        return n

    def rollback(self, index: int):
        """Decrement local counter (on send failure, before chain re-sync)."""
        acct = self.accounts[index]
        acct.nonce = max(0, acct.nonce - 1)

    # -- Startup verification --------------------------------------------------

    def verify_balances(self, rpc: RpcClient):
        """Check all accounts have non-zero balance. Fails fast on key mismatch."""
        for i, acct in enumerate(self.accounts):
            balance = rpc.get_balance(acct.address)
            if balance == 0:
                log.error(
                    "Account %d (%s) has ZERO balance! "
                    "Private key derivation may not match genesis allocation. "
                    "Verify keccak256 vs sha3_256 usage.",
                    i, acct.address,
                )
                sys.exit(1)
            log.info("Account %d: %s  balance=%.4f N42", i, acct.address, balance / 1e18)


# ---------------------------------------------------------------------------
# Statistics Tracker
# ---------------------------------------------------------------------------


@dataclass
class Stats:
    sent: int = 0
    failed: int = 0
    native_sent: int = 0
    erc20_sent: int = 0
    start_time: float = field(default_factory=time.time)

    def report(self, rpc: RpcClient):
        elapsed = time.time() - self.start_time
        try:
            block = rpc.block_number()
        except Exception:
            block = -1
        log.info(
            "STATS sent=%d (native=%d erc20=%d) failed=%d elapsed=%.0fs rate=%.2f tx/s block=%d",
            self.sent, self.native_sent, self.erc20_sent, self.failed,
            elapsed, self.sent / max(elapsed, 1), block,
        )


# ---------------------------------------------------------------------------
# Transaction Generator
# ---------------------------------------------------------------------------


class TxGenerator:
    """Builds, signs, and sends mixed transactions."""

    def __init__(self, rpc: RpcClient, acct_mgr: AccountManager, rate: int,
                 contract_hex_path: Optional[str] = None):
        self.rpc = rpc
        self.acct_mgr = acct_mgr
        self.rate = rate
        self.usdt_address: Optional[str] = None
        self.stats = Stats()
        self._contract_hex_path = contract_hex_path

    # -- Phase 1: ERC-20 deployment -------------------------------------------

    def deploy_erc20(self) -> str:
        """Deploy TestUSDT and return the contract address."""
        log.info("Phase 1: Deploying TestUSDT ERC-20 contract...")
        bytecode = _load_testusdt_bytecode(self._contract_hex_path)
        acct = self.acct_mgr.accounts[0]
        nonce = self.acct_mgr.get_and_increment(0)

        tx = {
            "type": 2,
            "chainId": CHAIN_ID,
            "nonce": nonce,
            "maxFeePerGas": MAX_FEE_PER_GAS,
            "maxPriorityFeePerGas": MAX_PRIORITY_FEE,
            "gas": DEPLOY_GAS,
            "value": 0,
            "data": "0x" + bytecode,
        }

        signed = Account.sign_transaction(tx, acct.private_key)
        raw_hex = "0x" + signed.raw_transaction.hex()
        tx_hash = self.rpc.send_raw_transaction(raw_hex)
        log.info("Deploy TX sent: %s", tx_hash)

        for attempt in range(120):
            if shutdown_event.is_set():
                raise SystemExit("Shutdown during deploy")
            try:
                receipt = self.rpc.get_receipt(tx_hash)
                if receipt is None:
                    time.sleep(2)
                    continue

                status = int(receipt.get("status", "0x0"), 16)
                if status != 1:
                    raise RuntimeError(f"Deploy reverted, receipt: {receipt}")

                raw_addr = receipt.get("contractAddress")
                if not raw_addr:
                    raise RuntimeError("Deploy receipt missing contractAddress")

                addr = to_checksum_address(raw_addr)
                log.info("TestUSDT deployed at %s (block %s)",
                         addr, receipt.get("blockNumber"))
                return addr
            except RuntimeError:
                raise
            except Exception:
                time.sleep(2)

        raise RuntimeError("Timeout waiting for deploy receipt (240s)")

    # -- Phase 1b: Distribute tokens to test accounts -------------------------

    def distribute_tokens(self):
        """Send USDT from deployer (account 0) to all other test accounts."""
        if not self.usdt_address:
            return
        log.info("Phase 1b: Distributing USDT to %d test accounts...",
                 NUM_ACCOUNTS - 1)
        contract = to_checksum_address(self.usdt_address)
        amount = 1_000_000 * 10**6  # 1M USDT each (6 decimals)

        for i in range(1, NUM_ACCOUNTS):
            if shutdown_event.is_set():
                return
            to_addr = self.acct_mgr.accounts[i].address
            calldata = _encode_erc20_transfer(to_addr, amount)
            acct = self.acct_mgr.accounts[0]
            nonce = self.acct_mgr.get_and_increment(0)
            tx = {
                "type": 2,
                "chainId": CHAIN_ID,
                "nonce": nonce,
                "maxFeePerGas": MAX_FEE_PER_GAS,
                "maxPriorityFeePerGas": MAX_PRIORITY_FEE,
                "gas": ERC20_TRANSFER_GAS,
                "to": contract,
                "value": 0,
                "data": "0x" + calldata.hex(),
            }
            signed = Account.sign_transaction(tx, acct.private_key)
            raw_hex = "0x" + signed.raw_transaction.hex()
            try:
                tx_hash = self.rpc.send_raw_transaction(raw_hex)
                log.info("  Sent 1M USDT to account %d: %s", i, tx_hash)
            except Exception as e:
                log.warning("  Failed to send USDT to account %d: %s", i, e)
        log.info("Waiting for distribution TXs to be mined...")
        time.sleep(8)
        log.info("USDT distribution complete")

    # -- Phase 2: Continuous transaction generation ----------------------------

    def _send_native_transfer(self, from_idx: int, to_idx: int) -> str:
        """Send a random native N42 transfer."""
        acct = self.acct_mgr.accounts[from_idx]
        to_addr = self.acct_mgr.accounts[to_idx].address
        amount = random.randint(10**16, 10**18)  # 0.01 - 1.0 N42

        nonce = self.acct_mgr.get_and_increment(from_idx)
        tx = {
            "type": 2,
            "chainId": CHAIN_ID,
            "nonce": nonce,
            "maxFeePerGas": MAX_FEE_PER_GAS,
            "maxPriorityFeePerGas": MAX_PRIORITY_FEE,
            "gas": TRANSFER_GAS,
            "to": to_addr,
            "value": amount,
            "data": "0x",
        }
        signed = Account.sign_transaction(tx, acct.private_key)
        raw_hex = "0x" + signed.raw_transaction.hex()
        tx_hash = self.rpc.send_raw_transaction(raw_hex)
        self.stats.native_sent += 1
        return tx_hash

    def _send_erc20_transfer(self, from_idx: int, to_idx: int) -> str:
        """Send an ERC-20 transfer. Falls back to native transfer if no contract."""
        if not self.usdt_address:
            return self._send_native_transfer(from_idx, to_idx)

        acct = self.acct_mgr.accounts[from_idx]
        to_addr = self.acct_mgr.accounts[to_idx].address
        # 1-1000 USDT (6 decimals)
        amount = random.randint(1, 1000) * 10**6
        calldata = _encode_erc20_transfer(to_addr, amount)

        nonce = self.acct_mgr.get_and_increment(from_idx)
        tx = {
            "type": 2,
            "chainId": CHAIN_ID,
            "nonce": nonce,
            "maxFeePerGas": MAX_FEE_PER_GAS,
            "maxPriorityFeePerGas": MAX_PRIORITY_FEE,
            "gas": ERC20_TRANSFER_GAS,
            "to": self.usdt_address,
            "value": 0,
            "data": "0x" + calldata.hex(),
        }
        signed = Account.sign_transaction(tx, acct.private_key)
        raw_hex = "0x" + signed.raw_transaction.hex()
        tx_hash = self.rpc.send_raw_transaction(raw_hex)
        self.stats.erc20_sent += 1
        return tx_hash

    def run(self):
        """Main generation loop with error recovery and backoff."""
        interval = 1.0 / self.rate
        consecutive_errors = 0
        last_nonce_sync = time.time()
        round_idx = 0

        log.info("Phase 2: Starting continuous TX generation at %d tx/sec", self.rate)

        while not shutdown_event.is_set():
            now = time.time()

            # Layer 3: Periodic full nonce sync
            if now - last_nonce_sync > NONCE_SYNC_INTERVAL:
                try:
                    self.acct_mgr.sync_all_nonces(self.rpc)
                    last_nonce_sync = now
                except Exception as e:
                    log.warning("Periodic nonce sync failed: %s", e)

            # Round-robin sender/receiver selection
            from_idx = round_idx % NUM_ACCOUNTS
            to_idx = (round_idx + 1) % NUM_ACCOUNTS
            round_idx += 1

            try:
                # One ERC-20 transfer every ERC20_TX_INTERVAL TXs (≈ every 2 blocks)
                if self.usdt_address and round_idx % ERC20_TX_INTERVAL == 0:
                    self._send_erc20_transfer(from_idx, to_idx)
                else:
                    self._send_native_transfer(from_idx, to_idx)

                self.stats.sent += 1
                consecutive_errors = 0

            except Exception as e:
                self.stats.failed += 1
                consecutive_errors += 1
                log.warning(
                    "TX error (consecutive=%d, from=%d): %s",
                    consecutive_errors, from_idx, e,
                )

                # Layer 2: Error-recovery nonce sync
                self.acct_mgr.rollback(from_idx)
                try:
                    self.acct_mgr.sync_nonce(from_idx, self.rpc)
                except Exception:
                    pass

                # Layer 1: Exponential backoff after too many consecutive errors
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS:
                    backoff = min(
                        BACKOFF_MIN * (2 ** (consecutive_errors - MAX_CONSECUTIVE_ERRORS)),
                        BACKOFF_MAX,
                    )
                    log.warning("Backing off %.1fs after %d consecutive errors",
                                backoff, consecutive_errors)
                    shutdown_event.wait(backoff)
                    try:
                        self.acct_mgr.sync_all_nonces(self.rpc)
                        last_nonce_sync = time.time()
                    except Exception:
                        pass
                    continue

            shutdown_event.wait(interval)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(description="N42 Testnet TX Load Generator")
    parser.add_argument("--rpc", default=DEFAULT_RPC,
                        help=f"RPC endpoint (default: {DEFAULT_RPC})")
    parser.add_argument("--rate", type=int, default=DEFAULT_RATE,
                        help=f"Transactions per second (default: {DEFAULT_RATE})")
    parser.add_argument("--no-erc20", action="store_true",
                        help="Skip ERC-20 deployment, native transfers only")
    parser.add_argument("--contract-hex",
                        help="Path to TestUSDT.hex (default: auto-detect)")
    args = parser.parse_args()

    log.info("N42 Testnet TX Load Generator starting")
    log.info("RPC: %s  Rate: %d tx/s  ERC-20: %s",
             args.rpc, args.rate, "disabled" if args.no_erc20 else "enabled")

    rpc = RpcClient(args.rpc)

    # Verify chain connection
    try:
        chain_id = rpc.chain_id()
        log.info("Connected to chain ID %d", chain_id)
        if chain_id != CHAIN_ID:
            log.warning("Expected chain ID %d, got %d", CHAIN_ID, chain_id)
    except Exception as e:
        log.error("Cannot connect to RPC at %s: %s", args.rpc, e)
        sys.exit(1)

    # Wait for block production
    log.info("Waiting for block production...")
    while not shutdown_event.is_set():
        try:
            block = rpc.block_number()
            if block > 0:
                log.info("Chain active at block %d", block)
                break
        except Exception:
            pass
        time.sleep(2)

    if shutdown_event.is_set():
        return

    # Generate accounts and verify balances
    acct_mgr = AccountManager()
    acct_mgr.verify_balances(rpc)
    acct_mgr.sync_all_nonces(rpc)

    # Create generator
    gen = TxGenerator(rpc, acct_mgr, args.rate,
                      contract_hex_path=args.contract_hex)

    # Phase 1: Deploy ERC-20
    if not args.no_erc20:
        try:
            gen.usdt_address = gen.deploy_erc20()
            gen.distribute_tokens()
            # Re-sync nonces after deploy + distribution TXs
            acct_mgr.sync_all_nonces(rpc)
        except SystemExit:
            return
        except Exception as e:
            log.error("ERC-20 deployment failed: %s", e)
            log.info("Continuing with native transfers only")

    # Background stats reporter
    def _stats_loop():
        while not shutdown_event.is_set():
            shutdown_event.wait(STATS_INTERVAL)
            if not shutdown_event.is_set():
                gen.stats.report(rpc)

    stats_thread = threading.Thread(target=_stats_loop, daemon=True)
    stats_thread.start()

    gen.run()
    gen.stats.report(rpc)
    log.info("TX generator shutdown complete")


if __name__ == "__main__":
    main()
