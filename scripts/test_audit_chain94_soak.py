import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('soak', Path(__file__).with_name('audit-chain94-soak.py'))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)

class SoakAuditTests(unittest.TestCase):
    def block(self):
        value = {field: '0x'+'11'*32 for field in audit.FIELDS}
        value.update(number='0x2a',gasUsed='0x0',transactions=[])
        return value

    def test_matching_continuous_blocks(self):
        block=self.block()
        self.assertEqual(audit.matching_blocks([block,copy.deepcopy(block)],42,block['parentHash']),block)

    def test_roots_and_parent_fail_closed(self):
        block=self.block();other=copy.deepcopy(block);other['stateRoot']='0x'+'22'*32
        with self.assertRaises(audit.h2.AuditError):audit.matching_blocks([block,other],42)
        with self.assertRaises(audit.h2.AuditError):audit.matching_blocks([block],42,'0x'+'33'*32)

    def test_missing_field_and_duplicate_transactions(self):
        block=self.block();del block['receiptsRoot']
        with self.assertRaises(audit.h2.AuditError):audit.matching_blocks([block],42)
        block=self.block();block['transactions']=['0x'+'44'*32]*2
        with self.assertRaises(audit.h2.AuditError):audit.matching_blocks([block],42)

    def test_node_adapter_binds_n42_status_to_canonical_qc(self):
        block=self.block();qc=dict(blockHash=block['hash'],view=9)
        status=dict(hasCommittedQc=True,validatorCount=7,commitQc=qc,latestCommittedBlockHash=block['hash'],latestCommittedView=9)
        with mock.patch.object(audit.h2,'rpc',side_effect=[status,block]), mock.patch.object(audit.h2,'read_committed_block',return_value=block), mock.patch.object(audit,'verify_qc',return_value={'verified':True}):
            self.assertEqual(audit.node_commit({'url':'local'},'n42-26',42,{},Path('unused')),(42,{'verified':True}))
        wrong=copy.deepcopy(block);wrong['hash']='0x'+'22'*32
        with mock.patch.object(audit.h2,'rpc',side_effect=[status,wrong]), mock.patch.object(audit.h2,'read_committed_block',return_value=block), mock.patch.object(audit,'verify_qc',return_value={'verified':True}):
            with self.assertRaises(audit.h2.AuditError):audit.node_commit({'url':'local'},'n42-26',42,{},Path('unused'))

    def test_rpc_commit_may_advance_after_the_sampled_head(self):
        block=self.block();qc=dict(blockHash=block['hash'],view=9)
        status=dict(hasCommittedQc=True,validatorCount=7,commitQc=qc,latestCommittedBlockHash=block['hash'],latestCommittedView=9)
        with mock.patch.object(audit.h2,'rpc',side_effect=[status,block]), mock.patch.object(audit.h2,'read_committed_block',return_value=block), mock.patch.object(audit,'verify_qc',return_value={'verified':True}):
            self.assertEqual(audit.node_commit({'url':'local'},'n42-26',41,{},Path('unused'))[0],42)

    def test_adapter_rejects_unknown_kind_and_qc_above_head(self):
        with self.assertRaises(audit.h2.AuditError):audit.node_commit({'url':'local'},'unknown',42,{},Path('unused'))
        header=self.qc_block();target=self.block()
        with mock.patch.object(audit.h2,'rpc',side_effect=[header,target]), mock.patch.object(audit.h2,'read_committed_block',return_value=target), mock.patch.object(audit,'verify_qc',return_value={'verified':True}):
            with self.assertRaises(audit.h2.AuditError):audit.node_commit({'url':'local'},'gov5',41,{},Path('unused'))

    def test_verifier_response_binds_exact_certificate_phase(self):
        qc = dict(view=9, blockHash='0x'+'11'*32)
        trusted = dict(profile='gov5legacy', validators=['key']*7, faultTolerance=2)
        proof = dict(verified=True, profile='gov5legacy', view=9, blockHash=qc['blockHash'],
                     validatorCount=7, faultTolerance=2, signerCount=5)
        def result(value):
            return mock.Mock(returncode=0, stdout=json.dumps(value))
        with mock.patch.object(audit.subprocess, 'run', return_value=result(proof)):
            audit.verify_qc(qc, trusted, Path('unused'))
            with self.assertRaises(audit.h2.AuditError):
                audit.verify_qc(qc, dict(trusted, certificatePhase='prepare'), Path('unused'))
        proof['certificatePhase']='prepare'
        with mock.patch.object(audit.subprocess, 'run', return_value=result(proof)):
            with self.assertRaises(audit.h2.AuditError):audit.verify_qc(qc, trusted, Path('unused'))
            audit.verify_qc(qc, dict(trusted, certificatePhase='prepare'), Path('unused'))

    def qc_block(self, packed=0x55):
        def field(data):return len(data).to_bytes(4,'little')+data
        qc=(9).to_bytes(8,'little')+field(bytes.fromhex('11'*32))+field(bytes.fromhex('22'*96))+field((7).to_bytes(2,'little')+bytes([packed]))
        block=self.block();block['extraData']='0x'+(b'N42H'+(10).to_bytes(8,'little')+qc+bytes(96)).hex()
        return block

    def test_gov5_header_qc_bitmap_is_little_endian(self):
        qc=audit.header_qc(self.qc_block(),7)
        self.assertEqual(qc['signers'],[True,False,True,False,True,False,True])
        self.assertEqual(qc['view'],9)
        self.assertEqual(qc['blockHash'],self.block()['hash'])

    def test_qc_padding_size_roster_and_trailing_rejected(self):
        for block,count in [(self.qc_block(0xd5),7),(self.qc_block(),6),(self.block(),7)]:
            with self.assertRaises(audit.h2.AuditError):audit.header_qc(block,count)
        block=self.qc_block();block['extraData'] += '00'
        with self.assertRaises(audit.h2.AuditError):audit.header_qc(block,7)


    def test_rust_checkpoint_uses_most_significant_bit_first(self):
        qc=dict(view=9,block_hash=self.block()['hash'],aggregate_signature=[17]*96,signers=[7,[0xaa]])
        converted=audit.checkpoint_qc(dict(last_committed_qc=qc),7)
        self.assertEqual(converted['signers'],[True,False,True,False,True,False,True])
        for bits in [[6,[0xaa]],[7,[0xab]],[7,[]]]:
            qc['signers']=bits
            with self.assertRaises(audit.h2.AuditError):audit.checkpoint_qc(dict(last_committed_qc=qc),7)


if __name__ == '__main__':unittest.main()
