#!/usr/bin/env python3
"""Settle a forbidden worker question without a fabricated user answer."""
import importlib.util
from pathlib import Path
import json
import unittest

spec = importlib.util.spec_from_file_location(
    'claude_admission_fixture', Path(__file__).with_name('claude-pre-admission-receipt-contract.py'))
admission = importlib.util.module_from_spec(spec)
spec.loader.exec_module(admission)
fixture = admission.fixture
ROLE_DENIAL = ('Only the orchestrator can ask the user. Send your question with '
               'orchestration_message target=lead; the orchestrator decides whether to contact the user.')
SDK = admission.SDK.replace(
    "accountInfo:async()=>{",
    """accountInfo:async()=>{
     if(options.systemPrompt&&fs.existsSync(options.cwd+'/.resume-dialog')){
      try{
       const result=await options.onUserDialog({dialogKind:'resume_return',toolUseID:'native-resume'},
        {requestId:'native-resume',signal:abort.signal});
       fs.writeFileSync(options.cwd+'/.dialog-result',JSON.stringify(result));
      }catch{
       fs.writeFileSync(options.cwd+'/.dialog-rejected','yes');
       await new Promise(()=>{});
      }
     }""")
SDK = SDK.replace(
    "if(text==='question'){",
    """if(text==='worker-question'){
     const decision=await options.canUseTool('AskUserQuestion',
      {questions:[{question:'Choose?',header:'Choice',options:[{label:'A',description:'First'}]}]},
      {toolUseID:'ask',signal:abort.signal});
     fs.writeFileSync(options.cwd+'/.permission-result',JSON.stringify(decision));
    }
    if(text==='question'){""")


class WorkerDialog(admission.PreAdmission):
    def setUp(self):
        original = admission.SDK
        admission.SDK = SDK
        try:
            super().setUp()
        finally:
            admission.SDK = original
        self.question_error = None

    def read(self):
        row = self.rows.get(timeout=10)
        if 'id' in row and 'method' in row:
            if row['method'] == 'item/tool/requestUserInput' and self.question_error is not None:
                self.write({'id': row['id'], 'error': {'code': -32600, 'message': self.question_error}})
            else:
                result = ({'decision': self.approval} if row['method'].endswith('requestApproval') else
                          {'answers': {'0': {'answers': self.question_answers}}}
                          if row['method'].endswith('requestUserInput') else
                          {'success': True, 'contentItems': [{'type': 'inputText', 'text': 'tool-ok'}]})
                self.write({'id': row['id'], 'result': result})
        return row

    def question_requests(self):
        return [row for row in self.notifications
                if row.get('method') == 'item/tool/requestUserInput']

    def test_role_denial_cancels_resume_dialog_then_admits_once(self):
        (self.root / '.resume-dialog').touch()
        self.question_error = ROLE_DENIAL
        turn = self.turn('fixture input', 'worker-resume')['turn']['id']
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(json.loads((self.root / '.dialog-result').read_text()), {'behavior': 'cancelled'})
        self.assertFalse((self.root / '.dialog-rejected').exists())
        self.assertEqual(len(self.question_requests()), 1)
        self.assertEqual(len(self.admissions()), 1)
        self.assertEqual(self.turn('fixture input', 'worker-resume')['turn']['id'], turn)
        self.assertEqual(len(self.admissions()), 1)

    def test_lead_resume_answer_keeps_the_original_choice(self):
        (self.root / '.resume-dialog').touch()
        self.question_answers = ['Keep full history']
        self.turn('fixture input', 'lead-resume')
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(json.loads((self.root / '.dialog-result').read_text()),
                         {'behavior': 'completed', 'result': 'continue'})
        self.assertEqual(len(self.admissions()), 1)

    def test_cancelled_worker_dialog_does_not_bypass_the_account_check(self):
        (self.root / '.resume-dialog').touch()
        (self.root / '.wrong-account').touch()
        self.question_error = ROLE_DENIAL
        self.turn('fixture input', 'wrong-account-resume')
        self.assertEqual(self.completed()['status'], 'failed')
        self.assertEqual(json.loads((self.root / '.dialog-result').read_text()), {'behavior': 'cancelled'})
        self.assertEqual(len(self.admissions()), 0)
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')

    def test_worker_tool_question_returns_a_permission_denial(self):
        self.question_error = ROLE_DENIAL
        self.turn('worker-question', 'denied-question')
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(json.loads((self.root / '.permission-result').read_text()),
                         {'behavior': 'deny', 'message': ROLE_DENIAL})
        self.assertEqual(len(self.question_requests()), 1)

    def test_unknown_question_error_cannot_admit_input(self):
        (self.root / '.resume-dialog').touch()
        self.question_error = 'The account connection ended'
        self.turn('fixture input', 'unknown-resume')
        self.assertEqual(self.completed()['status'], 'failed')
        self.assertTrue((self.root / '.dialog-rejected').exists())
        self.assertFalse((self.root / '.dialog-result').exists())
        self.assertEqual(len(self.admissions()), 0)
        self.assertEqual(self.history()[0]['startOutcome'], 'not_applied')


def load_tests(loader, tests, pattern):
    return loader.loadTestsFromNames([
        'test_role_denial_cancels_resume_dialog_then_admits_once',
        'test_lead_resume_answer_keeps_the_original_choice',
        'test_cancelled_worker_dialog_does_not_bypass_the_account_check',
        'test_worker_tool_question_returns_a_permission_denial',
        'test_unknown_question_error_cannot_admit_input',
    ], WorkerDialog)


if __name__ == '__main__':
    unittest.main()
