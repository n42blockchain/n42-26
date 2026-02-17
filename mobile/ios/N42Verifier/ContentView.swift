import SwiftUI

struct ContentView: View {
    @StateObject private var verifier = N42VerifierWrapper()
    @State private var serverHost = "192.168.1.100"
    @State private var serverPort = "9443"

    var body: some View {
        NavigationView {
            List {
                statusSection
                connectionSection
                statsSection
                if verifier.lastBlockInfo != nil {
                    latestBlockSection
                }
                logSection
            }
            .navigationTitle("N42 Verifier")
            .onAppear {
                verifier.initialize()
            }
        }
    }

    // MARK: - Status

    private var statusSection: some View {
        Section {
            HStack {
                Circle()
                    .fill(verifier.isConnected ? Color.green : Color.gray)
                    .frame(width: 12, height: 12)
                Text("Status: \(verifier.statusText)")
            }
            if !verifier.blsKeyHex.isEmpty {
                let key = verifier.blsKeyHex
                Text("BLS Key: \(key.prefix(8))...\(key.suffix(8))")
                    .font(.caption)
                    .monospaced()
                    .foregroundColor(.secondary)
            }
        }
    }

    // MARK: - Connection

    private var connectionSection: some View {
        Section("Connection") {
            TextField("Server", text: $serverHost)
                .disabled(verifier.isConnected)
                .autocapitalization(.none)
                .disableAutocorrection(true)

            TextField("Port", text: $serverPort)
                .disabled(verifier.isConnected)
                .keyboardType(.numberPad)

            HStack(spacing: 12) {
                Button("Connect") {
                    let port = UInt16(serverPort) ?? 9443
                    verifier.connect(host: serverHost, port: port)
                }
                .disabled(verifier.isConnected)
                .buttonStyle(.borderedProminent)

                Button("Disconnect") {
                    verifier.disconnect()
                }
                .disabled(!verifier.isConnected)
                .buttonStyle(.bordered)
            }
        }
    }

    // MARK: - Stats

    private var statsSection: some View {
        Section("Stats") {
            HStack {
                StatCard(title: "Blocks", value: "\(verifier.blocksVerified)")
                Spacer()
                StatCard(title: "Success", value: "\(verifier.successRate)%")
                Spacer()
                StatCard(title: "Avg Time", value: "\(verifier.avgTimeMs)ms")
            }
        }
    }

    // MARK: - Latest Block

    private var latestBlockSection: some View {
        Section("Latest Block") {
            if let info = verifier.lastBlockInfo {
                VStack(alignment: .leading, spacing: 4) {
                    Text("#\(info.blockNumber) \(info.blockHash.prefix(10))...\(info.blockHash.suffix(6))")
                        .monospaced()
                        .font(.subheadline)

                    HStack {
                        Image(systemName: info.receiptsRootMatch ? "checkmark.circle.fill" : "xmark.circle.fill")
                            .foregroundColor(info.receiptsRootMatch ? .green : .red)
                        Text("Receipts Root: \(info.receiptsRootMatch ? "Match" : "Mismatch")")
                    }

                    Text("Txs: \(info.txCount) | Witnesses: \(info.witnessAccounts)")
                        .font(.caption)

                    Text("Verify: \(info.verifyTimeMs)ms | Packet: \(String(format: "%.1f", Double(info.packetSizeBytes) / 1024.0))KB")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    // MARK: - Log

    private var logSection: some View {
        Section("Verification Log") {
            ForEach(verifier.verifyLog) { entry in
                HStack {
                    Text("#\(entry.blockNumber)")
                        .monospaced()
                        .font(.caption)
                    Spacer()
                    Image(systemName: entry.success ? "checkmark.circle.fill" : "xmark.circle.fill")
                        .foregroundColor(entry.success ? .green : .red)
                        .font(.caption)
                    Text("\(entry.verifyTimeMs)ms")
                        .monospaced()
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
        }
    }
}

struct StatCard: View {
    let title: String
    let value: String

    var body: some View {
        VStack {
            Text(value)
                .font(.title2)
                .monospaced()
                .fontWeight(.bold)
            Text(title)
                .font(.caption)
                .foregroundColor(.secondary)
        }
        .frame(minWidth: 80)
    }
}

struct ContentView_Previews: PreviewProvider {
    static var previews: some View {
        ContentView()
    }
}
