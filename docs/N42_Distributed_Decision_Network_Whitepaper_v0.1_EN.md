# N42 Distributed Decision Network Whitepaper v0.1

## A Distributed System-1 Decision Network for the AI Agent Era

**Version:** 0.1 Development & Validation Edition  
**Date:** 2026-09  
**Positioning:** Technical Whitepaper / Development, Testing & Validation Edition

> **N42 = Trust & Execution**  
> **N42-System1 = Fast Distributed Decision**  
> **System-2 Models = Reasoning & Generation**

> **Current Status:** N42 DDN has entered engineering development, model training, interface integration, node testing, protocol validation, and controlled rollout. This document describes a system that is being implemented and continuously iterated, not a conceptual planning exercise. At the same time, passing tests, completing individual modules, or having a runnable prototype is not presented as equivalent to a mature release. All modules remain subject to changes driven by performance, security, compatibility, and real-world usage feedback.

---

## Abstract

Current AI infrastructure is centered primarily on large generative models. Models such as GPT and Claude can perform complex reasoning, code generation, and natural-language expression, but their token-by-token generation mechanism makes them poorly suited to the enormous volume of continuously occurring, millisecond-scale decisions with limited answer spaces inside software systems.

Real software systems are not always asking:

> “Analyze this problem and write a report.”

More often, they need answers to questions such as:

> What category does this request belong to?  
> Is this abnormal?  
> Which tool should be called next?  
> Does this require human confirmation?  
> Is it worth invoking an expensive large model?  
> Is the current node state closer to a network, execution, or storage failure?

The Jev/System One Model introduced by TypeSafe AI demonstrates a different model interface: program state is provided as input, and the model returns predefined structured decisions, probabilities, and confidence values rather than arbitrary text. Its core idea can be summarized as: **unstructured state in, typed probabilistic decisions out**.

Building on this direction, the N42 Distributed Decision Network (**N42 DDN**) has already entered engineering development, protocol integration, model training, and node-side validation. The current direction is:

> **To extend System-1 from a centralized model API into distributed machine-decision infrastructure composed of blockchain nodes, local models, heterogeneous models, and verifiable receipts, while progressively bringing the system into usable form through continuous testing, calibration, and iteration.**

N42 DDN does not require every node to run the same model, and it does not define AI output as absolute truth. Instead, the network addresses a more fundamental set of questions:

**Who made a decision, which model and policy were used, what input commitment the decision corresponds to, whether multiple independent nodes produced sufficiently consistent results, whether the result has expired, who is accountable for it, and how the service can be verified, audited, and settled.**

N42’s existing technical foundation already includes Block-STM parallel execution, consensus-execution separation, the QMDB state engine, PeerDAS, WASM/Batch/Inference/Scheduler/Worker components, distributed storage, DID messaging, and a mobile-node build path. DDN is currently extending a new Decision Layer along these existing modules and is conducting integration testing across interfaces, nodes, models, and settlement flows rather than constructing an isolated AI network from scratch.

The core model of N42 DDN, **N42-System1**, is being developed, trained, and performance-tested using ModernBERT as an architectural baseline. It does not simply copy ModernBERT; instead, it is being continuously adapted around blockchain-node environments, distributed heterogeneous hardware, state classification, agent routing, risk assessment, and multi-node aggregation.

ModernBERT is an encoder-only architecture. Public versions include a Base model of roughly 149M parameters and a Large model of roughly 395M parameters, with native support for 8,192 tokens. It uses RoPE, GeGLU, unpadding, Flash Attention, and alternating attention with local attention in most layers and periodic global attention. This structure is particularly suitable for classification, retrieval, and code-related tasks.

N42-System1 retains the principle of a **small bidirectional encoder + high-efficiency classification**, while further transforming the model from a general-purpose text encoder into a:

> **Blockchain-Native Distributed Decision Model**

---

# 1. From Generative AI to Machine-Decision Infrastructure

Large generative models are best suited to open-ended tasks: complex reasoning, software development, document drafting, multi-step planning, explanation, and natural-language interaction between humans and machines.

Most machine actions in an AI Agent network, however, do not require natural-language generation.

For example, an N42 Agent may perform hundreds of decisions per day:

```text
Is this a balance query or a transaction request?
Should it call RPC or a smart contract?
Does the user need to confirm again?
Is this return value abnormal?
Which node currently has better response quality?
Is this request worth escalating to a higher-end model?
```

If every one of these decisions invokes a generative model with billions or hundreds of billions of parameters, the system incurs unnecessary cost, latency, and output-parsing complexity.

N42 therefore currently organizes and validates AI capabilities across three layers.

| Layer | Primary Responsibility | Technology |
|---|---|---|
| Deterministic Layer | Explicit facts, rules, verification, and execution | Conventional code, EVM, cryptography, state machines |
| System-1 Layer | High-frequency decisions with limited answer spaces | N42-System1, GLiClass, Jev, etc. |
| System-2 Layer | Open-ended complex reasoning and generation | Astra, Sol, Claude, etc. |

This separation is important.

**What is the balance?** does not require AI.

**Does this behavior resemble a scam?** may be appropriate for System-1.

**Why is this complex contract behavior abnormal, and what is the complete remediation plan?** is a System-2 task.

The goal of N42 DDN is not to “AI-enable everything,” but to select the lowest-cost, lowest-latency, sufficiently reliable computational method for each problem.

---

# 2. Design Goals

The first principle of N42 DDN is:

> **Decision is not Consensus.**

N42 blockchain consensus determines network-recognized state; an AI Decision is only a probabilistic judgment.

Therefore, N42-System1 does not participate in correctness decisions such as:

```text
signature validity
nonce
balance
state transition validity
block validity
consensus vote validity
gas accounting
cryptographic verification
```

These must continue to be handled by deterministic algorithms.

DDN targets:

```text
classification
routing
risk scoring
anomaly detection
agent action selection
content classification
tool selection
escalation decision
semantic policy evaluation
```

The network must remain model-agnostic, support heterogeneous nodes, prefer local execution, produce structured outputs, allow probability calibration, preserve traceability, support model replacement, degrade safely on failure, and remain isolated from the consensus-critical path.

---

# 3. Overall Architecture

The standard N42 DDN flow is:

```text
                   DApp / Agent / N42 App
                            │
                            ▼
                    DecisionRequest
                            │
                            ▼
                 N42 Decision Gateway
                            │
              Deterministic Pre-check
                            │
                            ▼
                    Decision Scheduler
                    /       |       \
                   /        |        \
                  ▼         ▼         ▼
          N42-System1     Jev      Other Models
          Local Nodes    Gateway      Providers
                  \         |         /
                   \        |        /
                    ▼       ▼       ▼
                  Decision Aggregator
                            │
                    disagreement?
                    /             \
                  No               Yes
                  │                 │
                  │           System-2 /
                  │           More Nodes /
                  │           Human Review
                  │                 │
                  └────────┬────────┘
                           ▼
                    DecisionReceipt
                           │
          ┌────────────────┼────────────────┐
          ▼                ▼                ▼
       Execute           Audit           Settlement
```

The most important change is:

**The model is no longer the network.**

A model is simply a `DecisionProvider`.

N42-System1 and Jev have been incorporated into a unified Provider abstraction. Other open-source models, enterprise-private models, and specialized industry models are also being validated for compatibility through the same interface.

---

# 4. N42-System1: Redesigning ModernBERT for Blockchain Nodes

## 4.1 Why ModernBERT Is Used as the Baseline

The value of ModernBERT is not in the name “BERT,” but in the fact that it re-demonstrates an important principle:

> For classification, retrieval, and semantic-understanding tasks, a well-optimized encoder-only model can offer a better performance-to-resource ratio than a generative model.

ModernBERT Base has roughly 149M parameters, while Large has roughly 395M parameters, with support for 8,192 tokens. It also uses local/global alternating attention, RoPE, GeGLU, unpadding, and Flash Attention to improve efficiency on long-context inference.

N42-System1 is extending this idea further into:

> **Local State + Periodic Global State**

This concept maps naturally onto distributed blockchain systems.

---

# 5. N42-System1 Model Architecture

N42-System1 is currently being developed as a **model family**, rather than as a single re-fine-tuned ModernBERT checkpoint.

Current development and validation cover three operating tiers:

| Version | Target Nodes | Design Goal |
|---|---|---|
| N42-System1 Edge | Mobile, IoT, low-power nodes | Very small model, INT8/Q4, local decisions |
| N42-System1 Node | PCs / ordinary nodes | Mainstream System-1 |
| N42-System1 Validator | High-performance servers | Long context, multi-task, multi-head |

Exact parameter counts are being narrowed through benchmarks, quantization results, and node-resource testing. Multiple model sizes ranging from tens of millions to several hundred million parameters are being validated, rather than locking any single size into a final release specification prematurely.

## 5.1 Blockchain State Encoding

A conventional NLP model sees text.

N42-System1 needs to see structured state:

```text
[CHAIN]
height=...
epoch=...

[TX]
method=approve
gas=...
value=...

[NODE]
peers=...
latency=...
missed_votes=...

[EXEC]
stm_abort_rate=...
qmdb_latency=...

[POLICY]
wallet_action
```

For this reason, **schema-aware serialization** has entered implementation and integration validation. It converts chain state, transactions, nodes, execution information, and policy fields into reproducible normalized inputs.

The model still consumes token sequences, but inputs are constrained by protocol rules so that different nodes can generate the same normalized representation from the same state.

---

# 6. Blockchain-Aware Alternating Attention

ModernBERT’s alternating attention can be abstracted as:

**Local → Local → Global → Local → Local → Global**

N42-System1 is developing and validating blockchain-aware attention so that the attention structure more closely reflects blockchain-local states such as transactions, contracts, peers, time windows, and execution batches.

Local attention prioritizes:

```text
same transaction
same contract
same peer
same time window
same execution batch
```

Periodic global attention then connects:

```text
network state
historical state
policy context
cross-component signals
```

This allows the model to first understand:

> “What happened in this group of QMDB logs?”

and then periodically establish:

> “How does it relate to the node’s network / execution / disk state over the last minute?”

This is not an existing ModernBERT feature. It is an **N42-System1 extension currently under implementation, testing, and tuning**. Its value continues to be evaluated using real N42 data and node workloads.

---

# 7. Typed Multi-Head Decision

N42-System1 is not designed primarily to “generate a sentence.”

A shared encoder can connect to multiple decision heads at the same time:

```text
                    N42 Encoder
                         │
        ┌────────────────┼────────────────┐
        ▼                ▼                ▼
     Fault Head       Risk Head      Routing Head
        │                │                │
 NETWORK 0.81        HIGH 0.04        RPC 0.93
 EXEC    0.12        MED  0.11        TOOL 0.05
 STORE   0.07        LOW  0.85        LLM 0.02
```

The following additional decision heads are also being validated:

```text
severity
abstention
need_more_data
need_system2
```

This allows a single encoder forward pass to produce several related decisions instead of rerunning a complete model for every question.

---

# 8. Calibration and Abstention

DDN should not ask only:

> What did the model choose?

It must also ask:

> Does the model know when it does not know?

For this reason, N42-System1 has incorporated calibration and abstention into its core training, testing, and acceptance criteria.

Instead of producing only:

```text
STORAGE
```

it produces:

```text
NETWORK      0.03
CONSENSUS    0.02
EXECUTION    0.12
STORAGE      0.78
UNKNOWN      0.05
```

and can return:

```text
ABSTAIN
```

A high-quality `ABSTAIN` is more valuable than a confidently wrong answer.

For asset, security, and automated-execution scenarios, current policy testing uses tiered rules similar to:

```text
confidence < T1 → more providers
confidence < T2 → System-2
high-risk task   → human confirmation
```

Thresholds cannot be determined by vendor marketing metrics. They must be calibrated on N42’s own real-world data.

---

# 9. Distributed Model Training Data

A major advantage of N42-System1 should come from **N42’s own long-term accumulation of real machine-behavior data**.

Candidate training data includes:

```text
node logs
CI failures
benchmark regression
RPC errors
P2P events
PeerDAS statistics
Block-STM conflict patterns
QMDB performance
EVM call metadata
ABI / method signatures
execution traces
known incident reports
code / issue / fix relationship
Agent routing results
```

Sensitive information must be processed before training.

Private keys, seed phrases, JWT secrets, and unauthorized user data must never enter the training corpus.

Federated / distributed dataset contribution has been added as an extended validation item in the data pipeline. It is currently being tested with respect to privacy boundaries, quality control, and node-contribution mechanisms, and is not presented as a stable release capability.

---

# 10. DecisionRequest Protocol

A DApp does not send an arbitrary Prompt directly to a model.

It creates a standardized `DecisionRequest`.

Conceptual data structure:

```text
DecisionRequest {
    version
    request_id

    task
    schema_id

    input_hash
    input_location?

    policy_hash
    policy_parameters

    model_requirements?

    privacy_mode

    quorum
    max_latency
    max_cost

    deadline
    nonce

    requester
    signature
}
```

`task` may be:

```text
agent.route
node.anomaly
wallet.risk
content.classify
spam.detect
iot.event
```

`input_hash` ensures that a Decision Receipt can be bound to a specific input.

The raw input itself **does not need to be placed on-chain**.

---

# 11. DecisionReceipt Protocol

After a node completes a decision, it returns:

```text
DecisionReceipt {
    version
    request_id

    provider_did

    model
    model_version
    model_hash

    policy_hash
    input_hash

    result
    probability_distribution
    confidence

    started_at
    completed_at
    latency

    expiry
    nonce

    evidence_commitment?

    provider_signature
}
```

`model_hash` identifies the checkpoint or Model Manifest actually used.

As a result, “N42-System1-v4” is no longer merely a string label.

The network can further bind it to:

```text
architecture
weights hash
tokenizer hash
quantization
training-data manifest
calibration version
runtime version
```

forming a **Model Manifest**.

---

# 12. Model Registration Protocol

The model-registration interface for Decision Providers is being developed and tested using the following structure:

```text
ModelManifest {
    model_id
    family
    version
    weights_hash
    tokenizer_hash

    supported_tasks[]
    supported_schemas[]

    precision
    context_limit

    calibration_manifest

    hardware_profile

    provider
    timestamp
    signature
}
```

This does not prove that a model is correct, but it does address another critical question:

> **At minimum, the network knows which model a node claims to be running.**

TEE, remote attestation, and ZKML have entered prototype and feasibility testing along the advanced-verification path, with the goal of strengthening evidence about the actual execution environment and model version.

These capabilities remain part of the advanced verification layer and are not presented in v0.1 as completed or production-stable functionality.

---

# 13. Decision Provider

DDN nodes are currently undergoing compatibility and interoperability testing across multiple model-provider types, including:

```text
N42-System1
GLiClass
fine-tuned ModernBERT
Jev Gateway
enterprise private classifier
industry-specific model
future System-One models
```

DDN is therefore a **model-agnostic network**.

This is important.

If N42 DDN depends for its success on one AI company existing forever, pricing never changing, and its API always being online, then it is not decentralized infrastructure.

Jev should serve within DDN as:

> **A strong cloud System-1 Provider and benchmark, not a protocol dependency.**

---

# 14. Decision Scheduler

The Scheduler does not simply select nodes at random.

It selects Providers based on:

```text
task capability
model compatibility
latency history
reliability
price
privacy
hardware
geography
reputation
requested quorum
```

For example, an ordinary Agent-routing request may be handled entirely by:

```text
N42-System1 Edge
```

A low-confidence security request may instead use:

```text
N42-System1 Node
+
Independent N42-System1 Node
+
Jev Gateway
```

A more complex request can then be escalated to:

```text
Sol / Astra / Claude
```

The most expensive models therefore sit at the top of the computational pyramid, not at the bottom.

---

# 15. Decision Quorum

Blockchains usually require deterministic consensus.

AI models are probabilistic by nature.

DDN therefore does not directly transplant traditional BFT consensus onto model outputs.

Instead, DDN uses a **Decision Quorum**.

For example:

```text
Provider A:
RISK_HIGH 0.91

Provider B:
RISK_HIGH 0.87

Provider C:
RISK_LOW 0.63
```

The Aggregator does not simply vote `2 : 1`.

It must also consider:

```text
model calibration quality
historical accuracy
task type
model correlation
Provider independence
model-family independence
```

Three nodes running the same checkpoint do not represent three truly independent information sources.

DDN therefore emphasizes:

> **Node diversity ≠ Model diversity.**

Where necessary, a request can require:

```text
2 independent providers
+
2 different model families
```

This is called a **Heterogeneous Decision Quorum**.

---

# 16. Aggregation Algorithm

The first generation can use a simple and auditable aggregation method:

```text
P(result) = Σ wi × Pi(result) / Σ wi
```

where `wi` comes from pre-published calibration/reputation parameters.

However, a “0.9 confidence” from one model may not have the same real-world meaning as “0.9 confidence” from another. Probabilities must therefore not be mixed directly without calibration.

If models strongly disagree:

```text
disagreement > threshold
```

the result should be escalated to:

```text
more nodes
→ heterogeneous models
→ System-2
→ human review
```

rather than forcing an answer.

---

# 17. Verification: What Is Being Verified?

“Verification” inside DDN must be defined precisely.

**Layer 1: Protocol Verification**  
Are the Request and Receipt formats, signatures, nonce, deadline, and hashes valid?

**Layer 2: Execution Identity Verification**  
Which Provider claims to have executed which Model Manifest?

**Layer 3: Multi-Node Result Consistency**  
Do independent Providers produce sufficiently similar outputs?

**Layer 4: Advanced Computation Proofs**  
TEE / remote attestation / ZKML, etc.

Even when computation proofs are used, they can only prove:

> “This model actually performed this computation on this input.”

They cannot prove:

> “This AI decision is necessarily correct in the real world.”

This is a fundamental security boundary of DDN.

---

# 18. Privacy Design

By default, N42 records only the following on-chain:

```text
input_hash
policy_hash
DecisionReceipt
settlement
```

rather than:

```text
raw prompt
private logs
personal information
documents
```

The input path currently supports and continues to validate the following modes:

- direct local inference;
- end-to-end encrypted delivery to a designated Provider;
- storage in an access-controlled data layer.

The following capabilities have entered the extended prototype and validation queue:

```text
TEE confidential inference
ZK proof
private retrieval
encrypted model execution
```

They remain optional layers.

---

# 19. Integration with Existing N42 Infrastructure

The existing N42 implementation already includes:

```text
internal/distributed/compute/{wasm,batch,inference,scheduler,worker}
```

as well as distributed storage and DID messaging.

DDN is therefore being extended, integrated, and regression-tested along existing modules.

## Compute Scheduler / Worker

Used for task discovery, node matching, and execution.

## DID

Used for Provider, Requester, and Model Publisher identities.

## QMDB

N42 is validating feasible implementations for using QMDB as data support for the Decision Registry, Receipt Index, and reputation state, while continuously adjusting indexing, historical-state, and query boundaries.

## PeerDAS

N42 is validating how PeerDAS can be adapted for large Model Manifests, dataset commitments, and related data-availability scenarios. This remains an experimental extension and has not been published as a stable interface.

## Block-STM

Can execute large volumes of Decision settlement, receipt registration, and DApp calls in parallel.

## WASM

Can be used for deterministic policy, aggregation, and business workflows.

N42 has already designed the ledger, compute, storage, identity, and messaging layers as a broader infrastructure base; DDN adds a new **Decision Layer** on top of that foundation.

---

# 20. Mobile Nodes and Edge Intelligence

One of the largest potential advantages of N42-System1 is not the data center, but mobile devices and ordinary PCs.

N42 already has a mobile-node build path that uses `gomobile bind` to generate iOS `.xcframework` and Android `.aar` outputs.

Deployment and resource testing on mobile nodes currently focuses on the quantized model form:

```text
N42-System1 Edge
```

responsible for:

```text
simple classification
spam detection
Agent routing
IoT events
local security alerts
peer quality
simple content tagging
```

Complex requests are escalated to stronger nodes.

The heterogeneous node configurations under validation are no longer limited to expensive GPU servers, but cover:

> **Mobile + PC + Server + Cloud Model**

working together as a heterogeneous Decision Network.

---

# 21. N42 App and AI Agent

N42 App is the most direct application entry point.

A user request enters:

```text
User
 ↓
N42 App
 ↓
Deterministic Rules
 ↓
N42-System1
```

If identified as `QUERY`, it calls RPC.

If identified as `SIMPLE_ACTION`, it calls a Tool.

If identified as `TRANSACTION`, it enters the wallet-confirmation flow.

Only if identified as `COMPLEX_REASONING` does it invoke System-2.

The resulting model is:

> **System-1 is present continuously; System-2 appears only when true reasoning and expression are required.**

---

# 22. Wallet Security

DDN is being used to validate auxiliary decision scenarios such as:

```text
whether user intent and the transaction are inconsistent
whether text attempts to induce the user to reveal a seed phrase
whether an approve operation behaves abnormally
whether the counterparty exhibits known risk patterns
```

AI, however, **must never hold final authority over assets**.

The standard path should be:

```text
Intent
 ↓
System-1 Risk Signal
 ↓
Deterministic Transaction Simulation
 ↓
User Confirmation
 ↓
Signature
 ↓
N42
```

The model must not:

```text
generate private keys
read seed phrases
automatically approve high-risk transactions
bypass user signatures
```

---

# 23. Intelligent Node Operations

The first DDN use case most suitable for production-oriented implementation is not finance, but node operations.

Inputs include:

```text
CPU
RAM
Peer count
Block latency
missed vote
STM abort rate
QMDB latency
PeerDAS status
logs
```

System-1 returns:

```text
HEALTHY
DEGRADED
CRITICAL
```

along with:

```text
NETWORK
CONSENSUS
EXECUTION
STORAGE
CONFIG
UNKNOWN
```

Routine problems can be matched directly to runbooks.

Complex or low-confidence problems are escalated to System-2 together with the relevant logs, while the escalation reason, cost, and outcome are continuously recorded.

One of the current testing priorities is to quantify whether this approach can significantly reduce the need for large models to read all logs while preserving recall for severe anomalies.

---

# 24. IoT and DePIN

For deployment across billions of devices, continuously running models with tens of billions of parameters is unrealistic.

Small encoder models are therefore being validated as a primary edge-node path.

For example:

```text
camera event
sensor anomaly
energy usage
vehicle telemetry
machine status
```

System-1 first classifies them as:

```text
normal
interesting
suspicious
urgent
```

Only a very small fraction of events enter high-cost computation.

The combination of DDN and DePIN is therefore not:

> “Every device has its own ChatGPT.”

It is:

> **Every device has the lowest-cost local decision capability it needs, and can invoke higher-level intelligence from the wider network when necessary.**

---

# 25. Economic Model

DDN computation is not free.

A complete task may include:

```text
request fee
inference fee
aggregation fee
verification fee
data fee
settlement fee
challenge fee
```

A Requester can set:

```text
max_cost
```

The Scheduler determines how to satisfy the request within that bound.

A simple task may use only one mobile or PC node.

A high-value task may pay more and require:

```text
3 providers
2 model families
high-quality calibration
System-2 fallback
```

The N42 settlement path is being validated at the interface and accounting levels as part of the DDN service loop.

However, v0.1 **does not predefine reward ratios, inflation models, or node yield**. These should be specified in a separate economic proposal only after real cost data has been established.

---

# 26. Reputation

Node Reputation must not simply mean:

> “Give more reputation to nodes that agree with the majority.”

Otherwise, the entire network can converge on collective error.

More meaningful data includes:

```text
availability
latency
protocol correctness
task completion
calibration error
verified outcome
dispute history
model diversity
```

For tasks whose ground truth becomes known later, the network can calculate:

```text
Brier Score
ECE
accuracy
precision
recall
```

to gradually form task-specific reputation.

Reputation must remain separated by task.

An excellent spam classifier does not automatically become an excellent financial-risk model.

---

# 27. Security Threats

DDN must address new attack surfaces beyond those of a traditional blockchain:

| Threat | Defense Direction |
|---|---|
| Sybil Provider | stake / DID / reputation / cost |
| Multiple nodes running the same malicious model | model-family diversity |
| Model version spoofing | Model Manifest hash |
| Replay | nonce + expiry + chain/domain separation |
| Prompt/State injection | schema separation + policy |
| Confidence manipulation | independent calibration |
| Collusion | heterogeneous quorum |
| Data leakage | local inference / encryption |
| Provider withholding | deadline + rescheduling |
| Model poisoning | signed model registry + evaluation |
| Correlated AI failure | deterministic fallback / diversity |

One of the most dangerous misconceptions is:

> “If three AIs agree, the answer must be correct.”

DDN must reject this assumption from the beginning.

---

# 28. Relationship Between N42-System1 and Jev

Jev is an important reference point.

It demonstrates that **high-speed typed probabilistic decision** is emerging as a distinct model category, using decision-oriented training and structured outputs.

N42-System1 has a different objective.

**Jev:** a centralized, general-purpose, cloud System-One Model.

**N42-System1:** an open / self-hostable model family trained for N42, agents, nodes, blockchain state, and edge devices.

**N42 DDN:** a model-agnostic distributed Decision Network in which many N42-System1 instances, Jev, and other models can operate together.

Therefore:

> **Jev is a model.  
> N42-System1 is a model family.  
> N42 DDN is the protocol and the network.**

---

# 29. Performance Evaluation

N42 DDN should not be evaluated using the assumption that “more parameters are always better.”

Core metrics should include:

```text
Accuracy
Recall
False Positive Rate
False Negative Rate
Calibration Error
Abstention Quality

P50 Latency
P95 Latency
P99 Latency

Decisions / second
Decisions / watt
Decisions / dollar

System-2 Escalation Rate
Network Disagreement Rate
Failure Recovery Time
```

The most important metric may ultimately be:

> **Useful Decisions per Dollar**

---

# 30. Relationship to N42 High Throughput

Project test records for N42 show n42-rs reaching 3.13M tx/s. This figure reflects a project test methodology and does not represent sustained throughput on a public mainnet under real-world network conditions.

DDN should not be interpreted as:

> “Therefore, N42 executes 3.13 million AI inferences per second.”

AI Decisions do not map one-to-one to blockchain transactions.

The correct pattern is:

```text
large volume of on-chain operations
     ↓
Event aggregation
     ↓
small number of meaningful decision states
     ↓
System-1
```

In other words:

**The chain processes state changes; System-1 interprets the events that actually require judgment.**

---

# 31. Why AI Must Not Enter the Consensus-Critical Path

This is one of the most important constraints in the entire whitepaper.

The following architecture is not allowed:

```text
TX
 ↓
AI API
 ↓
AI says valid
 ↓
Consensus
```

Reasons include:

```text
non-determinism
network latency
Provider outage
model upgrades
output differences across nodes
adversarial inputs
probabilistic error
```

N42 consensus must continue to function normally even when every model is completely unavailable.

Therefore:

> **AI may influence optimization; it must not influence validity.**

---

# 32. Block-STM Research

N42-System1 is conducting offline and asynchronous experiments for **Transaction Conflict Prediction**.

Based on:

```text
contract
method
historical access pattern
sender class
state locality
previous conflicts
```

it predicts transaction-batch conflicts.

The model provides only a:

```text
scheduler hint
```

Block-STM still performs the actual conflict detection.

The following design must not be used:

```text
every transaction → AI API → execution
```

because external model latency would destroy the high-performance execution path.

The correct pattern is:

```text
Historical data
 ↓
Async N42-System1
 ↓
Cached prediction
 ↓
Block scheduler hint
```

The feature is retained only if benchmarks show improvements in total throughput and tail latency.

---

# 33. PeerDAS / P2P Research

PeerDAS / P2P is undergoing similar validation for peer-quality prediction based on:

```text
latency
availability
bandwidth
DA success
response history
```

System-1 returns:

```text
preferred peers
```

Protocol verification remains unchanged.

A wrong model prediction may therefore reduce efficiency, but it must not compromise network security.

---

# 34. Development, Testing, Validation, and Rollout Status

## Phase 0 - Dataset & Benchmark | Ongoing

The N42 Decision Benchmark is being continuously built and expanded. It compares Rules, GLiClass, ModernBERT/N42-System1, and Jev under a unified methodology covering accuracy, recall, calibration error, latency, QPS, resource usage, and cost per decision.

## Phase 1 - Decision Gateway | Development & Shadow Mode Testing

Decision Gateway is undergoing read-only node/CI integration, event normalization, Provider routing, and failure-degradation testing. Shadow Mode does not alter production behavior; it is used to accumulate real comparative data and regression samples.

## Phase 2 - N42-System1 v0 | Training & Validation

N42-System1 v0 is being fine-tuned from ModernBERT and validated for quantization and node-side inference, with emphasis on fault classification, routing, escalation, and abstention.

## Phase 3 - N42-System1 v1 | Architectural Modification & Iterative Testing

Blockchain-aware encoder design, typed multi-head decisions, calibration, and schema-aware state encoding are under continuous development and comparative testing. The model architecture is repeatedly adjusted based on real node data.

## Phase 4 - N42 App / Agent | Integration Validation

System-1 Router, Agent tool routing, and Wallet Risk Signal are being integrated and validated under a strict boundary: they provide decision signals but do not hold authority to execute asset operations.

## Phase 5 - Distributed Providers | Protocol Prototype & Node Testing

Model Manifest registration, Provider identity, Signed DecisionReceipt, and multi-Provider compatibility are undergoing protocol-prototype development and node-interoperability testing.

## Phase 6 - Decision Quorum | Aggregation & Adversarial Validation

Heterogeneous model aggregation, reputation, challenge, abstention, and disagreement handling are being continuously validated through simulation, historical samples, and adversarial inputs.

## Phase 7 - Public DDN | Controlled Opening & Rollout Validation

DApp invocation, service discovery, permissions, billing, and N42 settlement are expanding into controlled test environments. External capabilities are being rolled out gradually with limits and rollback support; test availability is not presented as a mature public release.

## Phase 8 - Edge Network | Device Adaptation & Performance Validation

Quantized N42-System1 models on mobile, PC, and IoT nodes are being tested for model size, memory, power consumption, latency, and stability.

## Phase 9 - Core Optimization | Performance Experiments & Regression Validation

PeerDAS and Block-STM hints are being advanced as asynchronous optimization experiments. Any change that approaches a core execution path must pass throughput, tail-latency, security, and failure-fallback testing before it is retained.

The phases are at different levels of development, testing, and validation maturity and are managed through continuous **Go / Revise / Rollback** gates. Even completed modules remain subject to iteration driven by performance, security, compatibility, and real-world usage feedback; “completed” does not mean “frozen.”

---

# 35. N42 Distributed Decision Engine

The current engineering target can be summarized as:

```text
                    N42 NETWORK

 ┌──────────────────────────────────────────────┐
 │                                              │
 │             Deterministic Trust              │
 │                                              │
 │   Consensus / EVM / QMDB / PeerDAS / Crypto │
 │                                              │
 └──────────────────────────────────────────────┘
                       ▲
                       │
               DecisionReceipt
                       │
 ┌──────────────────────────────────────────────┐
 │                                              │
 │        Distributed Decision Network          │
 │                                              │
 │ Scheduler → Providers → Aggregator → Audit   │
 │                                              │
 └──────────────────────────────────────────────┘
                       ▲
                       │
 ┌─────────────────────┼────────────────────────┐
 │                     │                        │
 ▼                     ▼                        ▼
N42-System1            Jev                 Other Models
Local/Open            Cloud                  Open/Private
 │
 ├─ Mobile
 ├─ PC
 ├─ Validator
 └─ Data Center

                       │
             difficult / uncertain
                       ▼
             System-2 Reasoning
          Sol / Astra / Claude / ...
```

---

# 36. Longer-Term Significance

Over the past several decades, the Internet solved:

**How to transmit information.**

Blockchain solved:

**How mutually untrusted participants can jointly maintain state and value.**

Large AI models are beginning to solve:

**How machines understand, reason, and generate.**

But the AI Agent era still lacks one foundational layer:

> **How large numbers of machines can make continuous, low-cost decisions, while knowing who produced those decisions, which models were used, whether independent review occurred, and whether a result is trustworthy enough to trigger the next action.**

N42 DDN is implementing this layer as a network capability that is testable, reversible, and auditable.

It is not attempting to make the blockchain “run one super-AI.”

Quite the opposite.

The system directly targets a **highly heterogeneous intelligence ecosystem** in which small models on mobile devices, N42-System1 on PCs, enterprise-private models, System-One cloud models such as Jev, and higher-end System-2 models such as Astra, Sol, and Claude coexist.

What blockchain can provide is:

**identity, tasks, commitments, verification, coordination, audit, and settlement.**

What AI models provide is:

**decision.**

Together, they form a new kind of infrastructure:

> **Verifiable Distributed Machine Decision**

---

# 37. Conclusion

The goal of the N42 Distributed Decision Network is not to build yet another AI API, nor is it simply to “put model computation on-chain.”

It proposes a new computational hierarchy:

> **Deterministic programs solve problems that can be determined exactly;  
> System-1 models handle massive volumes of high-frequency, limited-answer-space ambiguous decisions;  
> System-2 models handle only the tasks that truly require complex reasoning and generation;  
> N42 organizes these different forms of intelligence into a network that is verifiable, auditable, and settleable.**

N42-System1 starts from ModernBERT’s efficient encoder architecture but is being redesigned for distributed blockchain nodes through:

**Blockchain State Encoding, Blockchain-Aware Local/Global Attention, Typed Multi-Head Decisions, Calibration, Abstention, Model Manifest, Edge Quantization, and Distributed Decision Quorum.**

ModernBERT has demonstrated that small encoders can achieve a strong performance-to-resource ratio for classification, retrieval, and code-related tasks. N42-System1 is now validating whether that advantage can be translated into the metrics required for long-running blockchain nodes: **decisions per second, decisions per watt, and useful decisions per dollar**. Model structure, quantization strategy, and node-deployment policy continue to be adjusted based on those results.

N42’s existing distributed compute, inference, Scheduler/Worker, mobile-node, DID, storage, and high-performance execution systems provide a concrete software foundation for this direction.

N42 is continuously evolving along the following path:

> **Distributed Ledger Network**

further into:

> **Distributed Compute Network**

and further into:

> **Distributed Decision Network**

so that people, AI Agents, mobile devices, servers, IoT devices, and smart contracts in the next-generation Internet can share not only data and value, but also a new foundational capability:

> **Trusted, low-cost, continuously available machine decisions.**

---

## Status Statement

This document is neither a conceptual planning document nor a mature-release announcement. Block-STM, QMDB, PeerDAS, mobile nodes, distributed compute/inference/scheduler/worker, storage, and DID messaging are technical foundations traceable to existing N42 materials. **N42-System1, Blockchain-Aware Attention, DecisionRequest/Receipt, Model Manifest, Decision Quorum, the DDN economic mechanism, and the public Decision Network are currently at different stages of development, prototyping, integration, testing, validation, or controlled rollout, and their maturity levels are not uniform. Capabilities that have not passed production-grade validation are not presented as stable mainnet functionality. Modules that are complete or runnable also continue to be iterated, modified, and upgraded based on performance, security, compatibility, and real-world usage feedback.**

---

## References and Internal Basis

1. N42 internal technical materials: *N42技术持续领先.pdf* and *N42技术持续领先_一页版_v2.pdf*. These materials document existing implementation paths including Block-STM, consensus-execution separation, QMDB, PeerDAS, mobile nodes, WASM/Batch/Inference/Scheduler/Worker, distributed storage, and DID messaging.
2. ModernBERT: Hugging Face Blog, *Smarter, Better, Faster, Longer: A Modern Bidirectional Encoder for Fast, Memory Efficient, and Long Context Finetuning and Inference*. https://huggingface.co/blog/modernbert
3. TypeSafe AI, *Introducing System One Models and Jev*. https://typesafe.ai/blog/introducing-system-one-models-and-jev
