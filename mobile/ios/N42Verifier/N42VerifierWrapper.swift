import Foundation
import Combine

/// Swift wrapper around the N42 mobile verifier FFI C API.
/// Uses @Published properties for SwiftUI data binding.
class N42VerifierWrapper: ObservableObject {

    // MARK: - Published state

    @Published var isConnected = false
    @Published var statusText = "Not initialized"
    @Published var blsKeyHex = ""
    @Published var blocksVerified: Int64 = 0
    @Published var successRate: Int64 = 0
    @Published var avgTimeMs: Int64 = 0
    @Published var lastBlockInfo: BlockInfo?
    @Published var verifyLog: [LogEntry] = []

    // MARK: - Private state

    private var ctx: OpaquePointer?
    private var verifying = false
    private let queue = DispatchQueue(label: "com.n42.verifier", qos: .userInitiated)

    // MARK: - Data types

    struct BlockInfo: Identifiable {
        let id = UUID()
        let blockNumber: Int64
        let blockHash: String
        let receiptsRootMatch: Bool
        let txCount: Int
        let witnessAccounts: Int
        let verifyTimeMs: Int64
        let packetSizeBytes: Int
    }

    struct LogEntry: Identifiable {
        let id = UUID()
        let blockNumber: Int64
        let success: Bool
        let verifyTimeMs: Int64
    }

    // MARK: - Lifecycle

    func initialize(chainId: UInt64 = 4242) {
        queue.async { [weak self] in
            guard let self = self else { return }

            let ptr = n42_verifier_init(chainId)
            guard ptr != nil else {
                DispatchQueue.main.async {
                    self.statusText = "Init failed"
                }
                return
            }

            self.ctx = ptr

            // Get public key
            var pubkeyBuf = [UInt8](repeating: 0, count: 48)
            if n42_get_pubkey(ptr, &pubkeyBuf) == 0 {
                let hex = pubkeyBuf.map { String(format: "%02x", $0) }.joined()
                DispatchQueue.main.async {
                    self.blsKeyHex = hex
                    self.statusText = "Ready"
                }
            }
        }
    }

    func connect(host: String, port: UInt16 = 9443) {
        guard let ctx = ctx else { return }

        DispatchQueue.main.async {
            self.statusText = "Connecting..."
        }

        queue.async { [weak self] in
            guard let self = self else { return }

            let result = host.withCString { hostPtr in
                n42_connect(ctx, hostPtr, port)
            }

            let connected = result == 0
            DispatchQueue.main.async {
                self.isConnected = connected
                self.statusText = connected ? "Connected" : "Connection failed"
            }

            if connected {
                self.startVerificationLoop()
            }
        }
    }

    func disconnect() {
        verifying = false
        guard let ctx = ctx else { return }

        queue.async { [weak self] in
            n42_disconnect(ctx)
            DispatchQueue.main.async {
                self?.isConnected = false
                self?.statusText = "Disconnected"
            }
        }
    }

    func destroy() {
        verifying = false
        if let ctx = ctx {
            n42_verifier_free(ctx)
            self.ctx = nil
        }
    }

    deinit {
        destroy()
    }

    // MARK: - Private

    private func startVerificationLoop() {
        verifying = true

        queue.async { [weak self] in
            guard let self = self, let ctx = self.ctx else { return }

            let bufSize = 4 * 1024 * 1024 // 4MB
            let buffer = UnsafeMutablePointer<UInt8>.allocate(capacity: bufSize)
            defer { buffer.deallocate() }

            while self.verifying {
                let packetSize = n42_poll_packet(ctx, buffer, bufSize)

                if packetSize > 0 {
                    let result = n42_verify_and_send(ctx, buffer, Int(packetSize))
                    let success = result == 0

                    // Update stats
                    self.updateStats()
                    self.updateLastBlockInfo()
                } else {
                    Thread.sleep(forTimeInterval: 0.1) // 100ms poll
                }
            }
        }
    }

    private func updateStats() {
        guard let ctx = ctx else { return }

        var statsBuf = [CChar](repeating: 0, count: 4096)
        let len = n42_get_stats(ctx, &statsBuf, statsBuf.count)
        guard len > 0 else { return }

        let json = String(cString: statsBuf)
        guard let data = json.data(using: .utf8),
              let dict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }

        DispatchQueue.main.async {
            self.blocksVerified = dict["blocks_verified"] as? Int64 ?? 0
            self.successRate = dict["success_rate"] as? Int64 ?? 0
            self.avgTimeMs = dict["avg_time_ms"] as? Int64 ?? 0
        }
    }

    private func updateLastBlockInfo() {
        guard let ctx = ctx else { return }

        var infoBuf = [CChar](repeating: 0, count: 8192)
        let len = n42_last_verify_info(ctx, &infoBuf, infoBuf.count)
        guard len > 0 else { return }

        let json = String(cString: infoBuf)
        guard let data = json.data(using: .utf8),
              let dict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }

        let info = BlockInfo(
            blockNumber: dict["block_number"] as? Int64 ?? 0,
            blockHash: dict["block_hash"] as? String ?? "",
            receiptsRootMatch: dict["receipts_root_match"] as? Bool ?? false,
            txCount: dict["tx_count"] as? Int ?? 0,
            witnessAccounts: dict["witness_accounts"] as? Int ?? 0,
            verifyTimeMs: dict["verify_time_ms"] as? Int64 ?? 0,
            packetSizeBytes: dict["packet_size_bytes"] as? Int ?? 0
        )

        let entry = LogEntry(
            blockNumber: info.blockNumber,
            success: info.receiptsRootMatch,
            verifyTimeMs: info.verifyTimeMs
        )

        DispatchQueue.main.async {
            self.lastBlockInfo = info
            self.verifyLog.insert(entry, at: 0)
            if self.verifyLog.count > 100 {
                self.verifyLog = Array(self.verifyLog.prefix(100))
            }
        }
    }
}
