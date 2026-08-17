//go:build ignore

// Exports gov5 block zero as an n42 finalized-range stream — the file
// N42_GOV5_GENESIS_BOOTSTRAP wants when bootstrapping an observer with
// N42_GOV5_QMDB_EXECUTION=1.
//
// Lives here but does NOT build here: it imports gov5's packages, so copy it
// into the gov5 checkout and run it there.
//
//     cp scripts/gov5-export-genesis-range.go ../n42-gov5/   # or C:
42
42-gov5
//     cd ../n42-gov5
//     go run ./gov5-export-genesis-range.go //         -db  <gov5-datadir>/chaindata //         -out <somewhere>/genesis-range.n42frng
//
// Pair it with gov5's own `cmd/n42-qmdb-export` for the QMDB checkpoint; the
// observer needs both. Why this exists: the finalized-range format is defined
// only on the Rust side (crates/n42-network/src/finalized_range.rs) and gov5
// has no encoder for it, so something has to bridge the two. Reading the block
// through gov5's own RLP rather than re-deriving the encoding is the whole
// point — the verifier checks keccak(header) against the published block hash,
// and a hand-rolled encoder is the easiest thing here to get subtly wrong.
// The self-check below fails loudly instead of writing a file that would only
// be rejected later.
//
// Wire format, as read back by decode_finalized_range_stream:
//
//   "N42FRNG" | chain_id u64le | genesis_hash 32
//   from u64le | to u64le | count u64le
//   per block: number u64le | hash 32 | parent 32 | state_root 32
//              | receipts_root 32 | tx_root 32
//              | u32le len + header RLP
//              | u32le len + block RLP
//              | u32le len + compact receipts (0 for an empty body)
//   blake3 over everything above (32)

package main

import (
	"context"
	"encoding/binary"
	"flag"
	"fmt"
	"os"

	"github.com/c2h5oh/datasize"
	"github.com/n42blockchain/N42/common/block"
	"github.com/n42blockchain/N42/lib/kv"
	mdbxkv "github.com/n42blockchain/N42/lib/kv/mdbx"
	"github.com/n42blockchain/N42/lib/rlp"
	log "github.com/n42blockchain/N42/lib/log/v3"
	"github.com/n42blockchain/N42/modules"
	"github.com/n42blockchain/N42/modules/rawdb"
	"lukechampine.com/blake3"
)

func fatalf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(1)
}

type framer struct{ buf []byte }

func (f *framer) raw(b []byte) { f.buf = append(f.buf, b...) }
func (f *framer) u64(v uint64) {
	var t [8]byte
	binary.LittleEndian.PutUint64(t[:], v)
	f.raw(t[:])
}
func (f *framer) hash32(h [32]byte) { f.raw(h[:]) }
func (f *framer) blob(b []byte) {
	var t [4]byte
	binary.LittleEndian.PutUint32(t[:], uint32(len(b)))
	f.raw(t[:])
	f.raw(b)
}

func main() {
	dbPath := flag.String("db", "", "gov5 chaindata directory")
	outPath := flag.String("out", "", "output finalized-range file")
	mapGB := flag.Int("map.gb", 8, "MDBX map size in GiB")
	flag.Parse()
	if *dbPath == "" || *outPath == "" {
		fatalf("usage: -db <chaindata> -out <file>")
	}

	modules.N42Init()
	kv.ChaindataTablesCfg = modules.N42TableCfg
	db, err := mdbxkv.NewMDBX(log.New()).Path(*dbPath).Label(kv.ChainDB).
		MapSize(datasize.ByteSize(*mapGB) * datasize.GB).Accede().Readonly().
		Open(context.Background())
	if err != nil {
		fatalf("open database: %v", err)
	}
	defer db.Close()
	tx, err := db.BeginRo(context.Background())
	if err != nil {
		fatalf("begin read-only transaction: %v", err)
	}
	defer tx.Rollback()

	genesisHash, err := rawdb.ReadCanonicalHash(tx, 0)
	if err != nil || genesisHash == ([32]byte{}) {
		fatalf("read genesis hash: %v", err)
	}
	cfg, err := rawdb.ReadChainConfig(tx, genesisHash)
	if err != nil || cfg == nil || cfg.ChainID == nil || !cfg.ChainID.IsUint64() {
		fatalf("read uint64 chain id: %v", err)
	}
	chainID := cfg.ChainID.Uint64()

	blk := rawdb.ReadBlock(tx, genesisHash, 0)
	if blk == nil {
		fatalf("read genesis block")
	}
	hdr := blk.Header()
	fullHeader, ok := hdr.(*block.Header)
	if !ok {
		fatalf("unexpected header type %T", hdr)
	}

	headerRLP, err := rlp.EncodeToBytes(fullHeader)
	if err != nil {
		fatalf("encode genesis header: %v", err)
	}
	blockRLP, err := rlp.EncodeToBytes(blk)
	if err != nil {
		fatalf("encode genesis block: %v", err)
	}

	// The verifier recomputes keccak(header) and compares; catch a mismatch
	// here rather than shipping a file that fails on the Rust side.
	if got := fullHeader.Hash(); got != genesisHash {
		fatalf("header RLP hashes to %x, not the canonical genesis %x", got, genesisHash)
	}

	f := &framer{}
	f.raw([]byte("N42FRNG\x01"))
	f.u64(chainID)
	f.hash32(genesisHash)
	f.u64(0)
	f.u64(0)
	f.u64(1)
	f.u64(0)
	f.hash32(genesisHash)
	f.hash32(fullHeader.ParentHash)
	f.hash32(blk.StateRoot())
	f.hash32(fullHeader.ReceiptHash)
	f.hash32(fullHeader.TxHash)
	f.blob(headerRLP)
	f.blob(blockRLP)
	f.blob(nil) // genesis carries no receipts

	sum := blake3.Sum256(f.buf)
	out := append(f.buf, sum[:]...)
	if err := os.WriteFile(*outPath, out, 0o644); err != nil {
		fatalf("write output: %v", err)
	}
	fmt.Printf("chain_id=%d genesis=%x state_root=%x header=%dB block=%dB total=%dB -> %s\n",
		chainID, genesisHash, blk.StateRoot(), len(headerRLP), len(blockRLP), len(out), *outPath)
}
