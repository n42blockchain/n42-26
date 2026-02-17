package com.n42.verifier

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONObject

data class LogEntry(
    val blockNumber: Long,
    val success: Boolean,
    val verifyTimeMs: Long,
)

class VerifierViewModel : ViewModel() {
    var isConnected by mutableStateOf(false)
        private set
    var serverHost by mutableStateOf("192.168.1.100")
    var serverPort by mutableStateOf("9443")
    var blsKeyHex by mutableStateOf("")
        private set
    var blocksVerified by mutableStateOf(0L)
        private set
    var successRate by mutableStateOf(0L)
        private set
    var avgTimeMs by mutableStateOf(0L)
        private set
    var lastBlockInfo by mutableStateOf<JSONObject?>(null)
        private set
    var logEntries by mutableStateOf<List<LogEntry>>(emptyList())
        private set
    var statusText by mutableStateOf("Disconnected")
        private set

    private var verifier: N42Verifier? = null
    private var verifying = false

    fun initialize() {
        viewModelScope.launch(Dispatchers.IO) {
            val v = N42Verifier.create() ?: return@launch
            verifier = v
            blsKeyHex = v.getPublicKeyHex()
            statusText = "Ready"
        }
    }

    fun connect() {
        val v = verifier ?: return
        val host = serverHost.trim()
        val port = serverPort.trim().toIntOrNull() ?: 9443

        viewModelScope.launch(Dispatchers.IO) {
            statusText = "Connecting..."
            val ok = v.connect(host, port)
            isConnected = ok
            statusText = if (ok) "Connected" else "Connection failed"

            if (ok) startVerificationLoop()
        }
    }

    fun disconnect() {
        verifying = false
        val v = verifier ?: return
        viewModelScope.launch(Dispatchers.IO) {
            v.disconnect()
            isConnected = false
            statusText = "Disconnected"
        }
    }

    private fun startVerificationLoop() {
        verifying = true
        viewModelScope.launch(Dispatchers.IO) {
            while (verifying && isActive) {
                val v = verifier ?: break
                val packet = v.pollPacket()

                if (packet != null) {
                    statusText = "Verifying..."
                    val success = v.verifyAndSend(packet)

                    // Update stats
                    v.getStats()?.let { stats ->
                        blocksVerified = stats.optLong("blocks_verified", 0)
                        successRate = stats.optLong("success_rate", 0)
                        avgTimeMs = stats.optLong("avg_time_ms", 0)
                    }

                    // Update last block info
                    v.lastVerifyInfo()?.let { info ->
                        lastBlockInfo = info
                        val entry = LogEntry(
                            blockNumber = info.optLong("block_number", 0),
                            success = info.optBoolean("receipts_root_match", false),
                            verifyTimeMs = info.optLong("verify_time_ms", 0),
                        )
                        logEntries = (listOf(entry) + logEntries).take(100)
                    }

                    statusText = if (success) "Connected" else "Verification error"
                } else {
                    delay(100) // 100ms poll interval
                }
            }
        }
    }

    override fun onCleared() {
        verifying = false
        verifier?.destroy()
    }
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                VerifierScreen()
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun VerifierScreen(vm: VerifierViewModel = viewModel()) {
    LaunchedEffect(Unit) { vm.initialize() }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("N42 Mobile Verifier") },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.primaryContainer
                )
            )
        }
    ) { padding ->
        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            // Status section
            item {
                StatusSection(vm)
            }

            // Connection section
            item {
                ConnectionSection(vm)
            }

            // Stats section
            item {
                StatsSection(vm)
            }

            // Latest block section
            item {
                LatestBlockSection(vm)
            }

            // Verification log header
            item {
                Text(
                    "Verification Log",
                    style = MaterialTheme.typography.titleMedium,
                    modifier = Modifier.padding(top = 8.dp)
                )
            }

            // Log entries
            items(vm.logEntries) { entry ->
                LogEntryRow(entry)
            }
        }
    }
}

@Composable
fun StatusSection(vm: VerifierViewModel) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (vm.isConnected) "\u25CF" else "\u25CB",
                    color = if (vm.isConnected) Color(0xFF4CAF50) else Color.Gray,
                    fontSize = 20.sp
                )
                Spacer(Modifier.width(8.dp))
                Text("Status: ${vm.statusText}", style = MaterialTheme.typography.bodyLarge)
            }
            if (vm.blsKeyHex.isNotEmpty()) {
                Spacer(Modifier.height(4.dp))
                Text(
                    "BLS Key: ${vm.blsKeyHex.take(8)}...${vm.blsKeyHex.takeLast(8)}",
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = FontFamily.Monospace
                )
            }
        }
    }
}

@Composable
fun ConnectionSection(vm: VerifierViewModel) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp)) {
            OutlinedTextField(
                value = vm.serverHost,
                onValueChange = { vm.serverHost = it },
                label = { Text("Server") },
                modifier = Modifier.fillMaxWidth(),
                enabled = !vm.isConnected
            )
            Spacer(Modifier.height(8.dp))
            OutlinedTextField(
                value = vm.serverPort,
                onValueChange = { vm.serverPort = it },
                label = { Text("Port") },
                modifier = Modifier.fillMaxWidth(),
                enabled = !vm.isConnected
            )
            Spacer(Modifier.height(12.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(
                    onClick = { vm.connect() },
                    enabled = !vm.isConnected
                ) {
                    Text("Connect")
                }
                OutlinedButton(
                    onClick = { vm.disconnect() },
                    enabled = vm.isConnected
                ) {
                    Text("Disconnect")
                }
            }
        }
    }
}

@Composable
fun StatsSection(vm: VerifierViewModel) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp)) {
            Text("Stats", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween
            ) {
                StatItem("Blocks Verified", "${vm.blocksVerified}")
                StatItem("Success Rate", "${vm.successRate}%")
                StatItem("Avg Time", "${vm.avgTimeMs}ms")
            }
        }
    }
}

@Composable
fun StatItem(label: String, value: String) {
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        Text(value, style = MaterialTheme.typography.titleLarge, fontFamily = FontFamily.Monospace)
        Text(label, style = MaterialTheme.typography.bodySmall, color = Color.Gray)
    }
}

@Composable
fun LatestBlockSection(vm: VerifierViewModel) {
    val info = vm.lastBlockInfo ?: return

    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp)) {
            Text("Latest Block", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.height(8.dp))

            val blockNum = info.optLong("block_number", 0)
            val blockHash = info.optString("block_hash", "")
            val rootMatch = info.optBoolean("receipts_root_match", false)
            val txCount = info.optInt("tx_count", 0)
            val witnesses = info.optInt("witness_accounts", 0)
            val verifyMs = info.optLong("verify_time_ms", 0)
            val packetSize = info.optInt("packet_size_bytes", 0)

            Text(
                "#$blockNum ${blockHash.take(10)}...${blockHash.takeLast(6)}",
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodyMedium
            )
            Text(
                "Receipts Root: ${if (rootMatch) "\u2705 Match" else "\u274C Mismatch"}",
                color = if (rootMatch) Color(0xFF4CAF50) else Color.Red
            )
            Text("Txs: $txCount | Witnesses: $witnesses")
            Text("Verify Time: ${verifyMs}ms | Packet: ${packetSize / 1024.0}KB")
        }
    }
}

@Composable
fun LogEntryRow(entry: LogEntry) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 2.dp),
        horizontalArrangement = Arrangement.SpaceBetween
    ) {
        Text(
            "#${entry.blockNumber} ${if (entry.success) "\u2705" else "\u274C"} ${entry.verifyTimeMs}ms",
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall
        )
    }
}
