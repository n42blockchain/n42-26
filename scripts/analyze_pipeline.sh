#!/bin/bash
# Analyze pipeline timing from validator logs
# Usage: ./scripts/analyze_pipeline.sh [log_dir]

LOG_DIR="${1:-/Users/jieliu/n42-testnet-data}"

echo "============================================"
echo "  N42 Pipeline Bottleneck Analysis"
echo "============================================"
echo ""

# 1. Leader build timing: serialize + compress + direct_push + eager_import
echo "=== LEADER BUILD PIPELINE ==="
echo "--- Serialize (JSON) ---"
grep "N42_LEADER_SERIALIZE" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  sed 's/.*ser_ms=\([0-9]*\).*/\1/' | sort -n | \
  awk '{a[NR]=$1; sum+=$1} END {
    if(NR>0) printf "  n=%d avg=%.0f p50=%s p95=%s max=%s ms\n", NR, sum/NR, a[int(NR*0.5)+1], a[int(NR*0.95)+1], a[NR]
  }'

echo "--- Compress (zstd) ---"
grep "N42_COMPRESS" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  sed 's/.*compress_ms=\([0-9]*\).*/\1/' | sort -n | \
  awk '{a[NR]=$1; sum+=$1} END {
    if(NR>0) printf "  n=%d avg=%.0f p50=%s p95=%s max=%s ms\n", NR, sum/NR, a[int(NR*0.5)+1], a[int(NR*0.95)+1], a[NR]
  }'

echo "--- Direct Push (to all peers) ---"
grep "N42_DIRECT_PUSH" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  sed 's/.*send_ms=\([0-9]*\).*/\1/' | sort -n | \
  awk '{a[NR]=$1; sum+=$1} END {
    if(NR>0) printf "  n=%d avg=%.0f p50=%s p95=%s max=%s ms\n", NR, sum/NR, a[int(NR*0.5)+1], a[int(NR*0.95)+1], a[NR]
  }'

echo "--- Leader Eager Import (new_payload) ---"
grep "eager import: new_payload accepted" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  grep -v follower | sed 's/.*np_elapsed=\([0-9]*\).*/\1/' | sort -n | \
  awk '{a[NR]=$1; sum+=$1} END {
    if(NR>0) printf "  n=%d avg=%.0f p50=%s p95=%s max=%s ms\n", NR, sum/NR, a[int(NR*0.5)+1], a[int(NR*0.95)+1], a[NR]
  }'

echo ""
echo "=== FOLLOWER IMPORT PIPELINE ==="
echo "--- Decompress + Deserialize ---"
grep "N42_DECOMPRESS" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  sed 's/.*decompress_ms=\([0-9]*\).*deser_ms=\([0-9]*\).*/\1 \2/' | \
  awk '{d+=$1; s+=$2; n++} END {
    if(n>0) printf "  n=%d avg_decompress=%.0f avg_deser=%.0f ms\n", n, d/n, s/n
  }'

echo "--- Follower Eager Import (new_payload) ---"
grep "follower eager import: new_payload accepted" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  sed 's/.*np_elapsed=\([0-9]*\).*/\1/' | sort -n | \
  awk '{a[NR]=$1; sum+=$1} END {
    if(NR>0) printf "  n=%d avg=%.0f p50=%s p95=%s max=%s ms\n", NR, sum/NR, a[int(NR*0.5)+1], a[int(NR*0.95)+1], a[NR]
  }'

echo ""
echo "=== CONSENSUS VOTING ==="
grep "view committed" "$LOG_DIR"/validator-0.log 2>/dev/null | tail -50 | \
  sed 's/.*R1_collect=\([0-9]*\)ms.*R2_collect=\([0-9]*\)ms.*total=\([0-9]*\)ms.*/\1 \2 \3/' | \
  awk '{r1+=$1; r2+=$2; tot+=$3; n++; if($1>mr1) mr1=$1; if($3>mt) mt=$3}
    END {
      if(n>0) printf "  n=%d R1_avg=%.0f R2_avg=%.0f total_avg=%.0f R1_max=%d total_max=%d ms\n", n, r1/n, r2/n, tot/n, mr1, mt
    }'

echo ""
echo "=== PIPELINE OVERLAP (last 50 blocks) ==="
grep "view committed" "$LOG_DIR"/validator-0.log 2>/dev/null | tail -50 | \
  sed 's/.*pipeline="role=\(.\) build=\([0-9-]*\)ms import=\([0-9-]*\)ms commit=\([0-9-]*\)ms.*/\1 \2 \3 \4/' | \
  awk '$1=="L" {lb+=$2; li+=$3; lc+=$4; ln++}
       $1=="F" {fb+=$2; fi_+=$3; fc+=$4; fn_++}
       END {
         if(ln>0) printf "  Leader:   n=%d avg_build=%dms avg_import=%dms avg_commit=%dms\n", ln, lb/ln, li/ln, lc/ln
         if(fn_>0) printf "  Follower: n=%d avg_build=%dms avg_import=%dms avg_commit=%dms\n", fn_, fb/fn_, fi_/fn_, fc/fn_
       }'

echo ""
echo "=== BLOCK SIZE DISTRIBUTION ==="
grep "N42_LEADER_SERIALIZE" "$LOG_DIR"/validator-*.log 2>/dev/null | \
  sed 's/.*tx_count=\([0-9]*\).*payload_kb=\([0-9]*\).*/\1 \2/' | sort -n -k1 | \
  awk '{tx[NR]=$1; kb[NR]=$2; stx+=$1; skb+=$2}
    END {
      if(NR>0) printf "  n=%d avg_tx=%d max_tx=%d avg_payload=%dKB max_payload=%dKB\n", NR, stx/NR, tx[NR], skb/NR, kb[NR]
    }'

echo ""
echo "=== TX FORWARD STATS ==="
for i in $(seq 0 6); do
  sent=$(grep -c "flushing tx forward" "$LOG_DIR/validator-$i.log" 2>/dev/null)
  recv=$(grep -c "received forwarded tx batch" "$LOG_DIR/validator-$i.log" 2>/dev/null)
  if [ "$sent" -gt 0 ] || [ "$recv" -gt 0 ]; then
    echo "  validator-$i: sent=$sent recv=$recv"
  fi
done

echo ""
echo "=== ERRORS / WARNINGS ==="
grep -c "error\|CRITICAL\|stall\|pipeline sync" "$LOG_DIR"/validator-*.log 2>/dev/null | grep -v ":0$"
