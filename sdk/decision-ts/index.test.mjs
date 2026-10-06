import test from 'node:test';
import assert from 'node:assert/strict';
import {keccak256,prepareProposal,validateQuote,submitProposal,statusName} from './index.mjs';

const router='0x1111111111111111111111111111111111111111';
const user='0x2222222222222222222222222222222222222222';
const proposalKey='0x'+'33'.repeat(32);

test('Ethereum Keccak known vectors',()=>{
  assert.equal(keccak256(''),'0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470');
  assert.equal(keccak256('abc'),'0x4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45');
});
test('proposal commits exact UTF-8 JSON bytes',()=>{
  const proposal=prepareProposal({title:'公开提案',body:'材料',proposalKey});
  assert.equal(new TextDecoder().decode(proposal.stateBytes),proposal.state);
  assert.equal(proposal.inputHash,keccak256(proposal.stateBytes));
  assert.throws(()=>prepareProposal({title:'',body:'x',proposalKey}));
});
test('quote binds router, user, input, template, and expiry',()=>{
  const proposal=prepareProposal({title:'A',body:'B',proposalKey});
  const quote={requester:router,consumer:router,refundTo:user,inputHash:proposal.inputHash,templateId:'7',deadline:2000,quoteExpiry:1500,fee:'100',signerVersion:1};
  const context={router,refundTo:user,templateId:7,now:1000};
  assert.equal(validateQuote(quote,proposal,context),true);
  assert.throws(()=>validateQuote({...quote,inputHash:'0x'+'00'.repeat(32)},proposal,context));
  assert.throws(()=>validateQuote({...quote,refundTo:router},proposal,context));
  assert.throws(()=>validateQuote(quote,proposal,{...context,now:1501}));
});
test('wallet receives exact encoded request arguments',async()=>{
  const proposal=prepareProposal({title:'A',body:'B',proposalKey});
  const quote={requester:router,consumer:router,refundTo:user,inputHash:proposal.inputHash,templateId:7,deadline:2000,quoteExpiry:1500,fee:'100',signerVersion:1};
  const realNow=Date.now;
  Date.now=()=>1_000_000;
  try {
    const wallet={writeContract:async args=>{assert.equal(args.value,100n);assert.equal(args.functionName,'submit');assert.deepEqual(args.args[1],proposal.stateBytes);assert.equal(args.args[2].fee,100n);return '0xabc';}};
    assert.equal(await submitProposal(wallet,{router,refundTo:user,templateId:7,quote,signature:'0x01',proposal}),'0xabc');
  } finally {Date.now=realNow;}
  assert.equal(statusName(2),'Review');
});
