import copy
import importlib.util
from pathlib import Path
import unittest

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

    def evidence(self):
        return dict(blockHash=self.block()['hash'],signerCount='0x7',view='0x9',signers='0x55',aggregateSignature='0x'+'11'*96)

    def test_gov5_bitmap_is_little_endian(self):
        qc=audit.evidence_qc(self.evidence(),self.block(),7)
        self.assertEqual(qc['signers'],[True,False,True,False,True,False,True])
        self.assertEqual(qc['view'],9)

    def test_evidence_wrong_hash_roster_padding_or_size_rejected(self):
        for field,value in [('blockHash','0x'+'22'*32),('signerCount','0x6'),('signers','0xd5'),('signers','0x')]:
            evidence=self.evidence();evidence[field]=value
            with self.assertRaises(audit.h2.AuditError):audit.evidence_qc(evidence,self.block(),7)

if __name__ == '__main__':unittest.main()
