"""Selected-scope metadata/process fakes; no native CLI, SQLite or photo access."""
import ast
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import types
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
def load(name, filename):
    spec=importlib.util.spec_from_file_location(name,ROOT/'scripts'/filename)
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module
C=load('selected_contract','lightroom_phase_contract.py')
W=load('selected_controller','lightroom_phase_control.py');W.C=C
A=load('selected_auditor','lightroom_selected_packets_audit.py')


class SelectedPackets(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name).resolve()
        self.run=self.root/'run';self.run.mkdir();self.control=self.root/'control';self.control.mkdir()
        for name in ['reports','plans','commands','steps','captures']: (self.run/name).mkdir()
        for path in [self.run/'runner.lock',self.control/'owner.lock']:path.touch()
        self.keys=[C.sha(str(i).encode()) for i in range(3)]
        self.members=[{'revision_id':'r'+str(i)} for i in range(3)]
        self.families=[{'id':'A','evidence_digest':'a'*64,'suggested':'r0','members':self.members[:2]},
                       {'id':'B','evidence_digest':'b'*64,'suggested':'r2','members':self.members[2:]}]
        main={'automatic_selection':False,'migration_executed':False,'inventory_complete':True,
              'inventory_delta':self.stable(),'candidate_count':3,'families':{'families':self.families},'outcomes':[]}
        for i,key in enumerate(self.keys):
            source={'encoding':'UnixBytes','units':list(str(self.root/f'never-read-{i}').encode())}
            main['outcomes'].append({'key':key,'candidate':{'path':source},'capture_path':f'capture-{i}',
                                    'capture':{'revision_id':f'r{i}'},'inspection':{'revision':f'r{i}'}})
        self.main_ref=self.put(self.run/'reports/main-review.json',main)
        full_request={'main_review_sha256':self.main_ref['sha256'],'automatic_selection':False,'requests':[
            {'candidate_key':k,'revision':f'r{i}','family_evidence_digest':('a' if i<2 else 'b')*64,
             'role':'additional_ambiguity_evidence','reason':'synthetic preserve'} for i,k in enumerate(self.keys)]}
        rid=C.sha(C.encoded(full_request));plan=str(self.run/'plans'/('final-'+rid));outcomes=[]
        for i,k in enumerate(self.keys):
            capture={'state':'captured','raw_byte_retention':'complete','sqlite_consistency':'consistent_default_sqlite','revision_id':f'r{i}'}
            outcomes.append({'key':k,'source':main['outcomes'][i]['candidate']['path'],'full_requested':True,
                             'full':{'ok':True,'capture':capture,'capture_path':f'capture-{i}'},
                             'inspection':{'revision':f'r{i}','capture_path':f'capture-{i}'}})
        full={'automatic_selection':False,'migration_executed':False,'request':full_request,'request_id':rid,
              'plan':plan,'inventory_delta':self.stable(),'outcomes':outcomes,'ending_inventory_command':1,
              'outcome_counts':{'candidates':3,'full_requested':3,'full_capture_failures':0,'inspection_failures':0,'main_only_members':0}}
        self.full_ref=self.put(self.run/'reports'/('full-review-'+rid+'.json'),full)
        paths={'automatic_selection':False,'migration_executed':False,'full_review_sha256':self.full_ref['sha256'],
               'inventory_delta':self.stable(),'outcome_counts':{'requested_members':3,'failures':0},
               'families':{'families':self.families},'outcomes':[{'key':k,'revision':f'r{i}','report':{'revision_id':f'r{i}'},
                    'pages':{n:self.empty_page() for n in ['paths','packets','metadata-conflicts','issues']}} for i,k in enumerate(self.keys)]}
        self.paths_ref=self.put(self.run/'reports'/('paths-review-'+self.full_ref['sha256']+'.json'),paths)
        runner=C.reference(ROOT/'scripts/run_lightroom_inspection.py');helper=C.reference(ROOT/'scripts/lightroom_generation.py')
        self.config={'stdout_caps_bytes':{'page':8*1024**2,'aggregate':8*1024**2,'document':8*1024**2},'stderr_cap_bytes':1024**2,'catalog_root':str(self.root/'never-read-catalogs'),'page_limit':1000,'maximum_calls_per_revision':10,
                     'exclusive_output':str(self.run),'automatic_choose':False,'automatic_migration':False,
                     'closed_application_evidence':None,'source_commit':'1'*40,'tested_binary_sha256':'2'*64}
        self.binding={'source':'1'*40,'binary_sha256':'2'*64,'binary_bytes':10,'driver_sha256':runner['sha256'],
                      'generation_driver_sha256':helper['sha256'],'config_sha256':C.sha(C.encoded(self.config))}
        runtime=self.root/'python';runtime.write_text('synthetic-not-executed')
        supervisor=self.root/'supervisor.py';supervisor.write_text('# synthetic, never executed\n')
        funding=self.root/'funding.py';funding.write_text('# synthetic, never executed\n')
        self.recipe={'protocol':1,'phase':'packets','run':str(self.run),'control':str(self.control),
                     'attempt_id':'00000000-0000-4000-8000-000000000001','input':self.full_ref,'baseline':self.full_ref,
                     'paths_review':self.paths_ref,'config':self.put(self.run/'config.json',self.config),
                     'binding':self.put(self.run/'binding.json',self.binding),
                     'code':{'runner':runner,'helper':helper,'controller':C.reference(ROOT/'scripts/lightroom_phase_control.py'),
                             'contract':C.reference(ROOT/'scripts/lightroom_phase_contract.py'),'python':C.reference(runtime),
                             'supervisor':C.reference(supervisor),'funding_guard':C.reference(funding)},
                     'expected_next_command':2,'journal':self.put(self.run/'journal.json',{'next_command':2}),
                     'pause':{'kind':'absent'},'memory':{'python_process_rss_bytes':536870912,'native_process_rss_bytes':1073741824,'combined_owned_rss_bytes':1610612736}}
        self.attempt=self.control/'attempts'/self.recipe['attempt_id'];self.attempt.mkdir(parents=True)
        self.paths_review=self.put(self.root/'paths-review-proof.json',{'status':'PASS','output':self.paths_ref,
            'binding':self.recipe['binding'],'full_anchor':{'output':self.full_ref}})
        self.selection={'protocol':1,'kind':'selected_current_catalog_packets','full':self.full_ref,'paths':self.paths_ref,
                        'paths_review':self.paths_review,'proposal':self.put(self.root/'proposal.json',{'status':'SYNTHETIC_PROPOSED'}),
                        'selected':[{'family_id':f,'family_evidence_digest':f.lower()*64,'candidate_key':self.keys[i],'revision':f'r{i}',
                                     'reason':'explicit synthetic user choice'} for f,i in [('A',0),('B',2)]],
                        'excluded':[{'candidate_key':self.keys[1],'revision':'r1','disposition':'external_packets_not_selected'}]}
        self.reauthorize()
        self.canonical_setup()
        native=self.root/'successor';native.write_bytes(b'not executable; metadata fixture')
        self.native={'path':str(native),'bytes':native.stat().st_size,'sha256':hashlib.sha256(native.read_bytes()).hexdigest(),'source':'3'*40}
        build=self.put(self.root/'build.json',{'status':'BUILT','native':self.native})
        qualification=self.put(self.root/'qualification.json',{'status':'PASS','native':self.native,'build':build,
            'base_binding':self.recipe['binding'],'base_driver':runner,'schema_version':3,'reviewer':'synthetic independent reviewer',
            'evidence':[self.put(self.root/'tiny-gate.json',{'status':'SYNTHETIC_TEST_ONLY'})],
            'assertions':{'cli_schema_compatible':True,'unchanged_packet_values':True,'stability_and_fallback_verified':True,'source_preservation_verified':True}})
        self.profile={'protocol':1,'kind':'qualified_selected_packets_native','base_binding':self.recipe['binding'],
                      'base_driver':runner,'native':self.native,'build':build,'qualification':qualification}
        self.recipe['native_execution_profile']=self.put(self.root/'native-profile.json',self.profile)
        self.recipe['funding']={'synthetic':'supplied by focused test'}
        self.previous_setup()

    def tearDown(self):self.temp.cleanup()
    def put(self,path,value):path.write_bytes(C.encoded(value));return C.reference(path)
    def stable(self):return {'before_complete':True,'after_complete':True,'added':[],'removed':[],'changed':[]}
    def empty_page(self):return {'rows':0,'counts':{},'last_sequence':0,'pages':[],'content_sha256':hashlib.sha256().hexdigest(),'identity_sha256':hashlib.sha256().hexdigest(),'available_reference_bytes':0}
    def context(self):return W.native_context(self.recipe,C.context(self.recipe),self.binding)
    def reauthorize(self):
        self.selection.pop('authorization',None)
        source=self.put(self.root/'user-message.json',{'role':'user','message_id':'synthetic-local-message','text':'Use A current and B current, not the backup.'})
        self.selection['authorization']=self.put(self.root/'user-authorization.json',{'status':'USER_AUTHORIZED',
            'selection_body_sha256':C.sha(C.encoded(self.selection)),'source_message':source,'quote':'Use A current and B current','reviewer':'synthetic receipt reviewer'})
        self.recipe['packet_selection']=self.put(self.root/'selection.json',self.selection)
    def canonical_setup(self):
        source=(ROOT/'scripts/run_lightroom_inspection.py').read_text();tree=ast.parse(source)
        nodes=[n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name in W.REPLAY_FUNCTIONS]
        helper=self.root/'json-profile.py';helper.write_text('import json\n'+'\n'.join(ast.get_source_segment(source,n).replace('cap=16*MIB', 'cap=16777216') for n in nodes)+'\n')
        assertions={k:True for k in ['canonical_bytes_equal','bounded_fast_path','fallback_preserved','read_json_equivalent','raw_bytes_released_before_parse']}
        # Match the repository's public protocol2 assertion names exactly.
        base_review={'status':'PASS','author':'synthetic','base_driver':self.recipe['code']['runner'],
                     'runtime':self.recipe['code']['python'],'helper':C.reference(helper),'assertions':assertions}
        profile={'protocol':2,'kind':'canonical_hash_and_replay_json_override','base_driver':self.recipe['code']['runner'],
                 'runtime':self.recipe['code']['python'],'helper':C.reference(helper),'functions':W.REPLAY_FUNCTIONS,
                 'equivalence_review':self.put(self.root/'python-review.json',base_review)}
        self.recipe['canonical_hash_profile']=self.put(self.root/'python-profile.json',profile)
    def previous_setup(self):
        old=copy.deepcopy(self.recipe)
        for k in ['packet_selection','native_execution_profile']:old.pop(k)
        old['phase']='paths';old['paths_review']={'kind':'not_applicable'}
        old['attempt_id']='00000000-0000-4000-8000-000000000002';old['previous']={'fixture':'historical untouched'}
        self.old_attempt=self.control/'attempts'/old['attempt_id'];self.old_attempt.mkdir()
        old_ref=self.put(self.old_attempt/'recipe.json',old)
        process=self.put(self.old_attempt/'process.json',{'pid':234})
        proof=self.put(self.old_attempt/'execution-profile.json',W.execution_profile_value(old,self.old_attempt))
        consumed=self.put(self.old_attempt/'execution-profile-consumed.json',{'execution_profile':proof,
            'helper':C.document(old['canonical_hash_profile'])['helper'],'pid':234})
        phase=self.put(self.run/'reports/phase-old.json',{'phase':'paths','binding':self.binding,
            'input':old['input']['path'],'status':'review_artifact_returned_not_acceptance','output':self.paths_ref['path']})
        self.old_result={'status':'review_returned_not_acceptance','ownership_status':'observed_owned_processes_reaped',
            'root_reaped':True,'new_command_failures':[],'exit_code':0,'cleanup':None,'journal':self.recipe['journal'],
            'next_command':2,'phase':phase,'phase_name':'paths','phase_input':self.full_ref,'output':self.paths_ref,
            'execution_profile':proof,'execution_profile_consumed':consumed}
        old_result_ref=self.put(self.old_attempt/'result.json',self.old_result)
        review=self.put(self.root/'old-review.json',{'status':'PASS','result':old_result_ref,'binding':self.recipe['binding'],
            'output':self.paths_ref,'execution_profile':proof})
        self.recipe['previous']={'result':old_result_ref,'review':review}
        self.put(self.control/'current.json',{'attempt_id':old['attempt_id']})
        self.recipe['packet_transition']=self.put(self.root/'transition.json',W.packet_transition_expected(self.recipe,old_ref))
    def execution_files(self):
        self.put(self.attempt/'recipe.json',self.recipe)
        self.put(self.attempt/'native-execution.json',W.native_execution_value(self.recipe,self.attempt))

    def test_exact_partition_order_and_old_evidence_preserved(self):
        before={p:p.read_bytes() for p in [Path(self.full_ref['path']),Path(self.paths_ref['path']),self.run/'binding.json']}
        ctx=self.context();self.assertEqual(list(ctx['requested']),[self.keys[0],self.keys[2]])
        self.assertEqual(len(ctx['all_requested']),3);self.assertEqual(len(ctx['excluded']),1)
        self.assertIn(self.recipe['packet_selection']['sha256'],ctx['output'])
        self.assertEqual(ctx['tag'][0],'packets-selected')
        self.assertEqual(before,{p:p.read_bytes() for p in before})
    def test_selection_missing_duplicate_foreign_stale_and_incomplete_fail(self):
        original=copy.deepcopy(self.selection)
        changes=[lambda x:x['selected'].pop(),lambda x:x['selected'].append(x['selected'][0]),
                 lambda x:x['selected'][0].update(revision='foreign'),lambda x:x['selected'][0].update(family_evidence_digest='0'*64),
                 lambda x:x['excluded'].clear(),lambda x:x['excluded'][0].update(candidate_key=self.keys[0]),
                 lambda x:x['selected'][0].update(reason=''),lambda x:x['selected'][0].update(reason=' \t ')]
        for change in changes:
            self.selection=copy.deepcopy(original);change(self.selection);self.reauthorize()
            with self.assertRaises(ValueError):self.context()
    def test_proposal_or_generated_role_is_not_user_authorization(self):
        auth=C.document(self.selection['authorization'])
        for mutate in [lambda a:a.update(status='PROPOSED'),lambda a:a.update(selection_body_sha256='0'*64),
                       lambda a:a.update(quote='unrelated statement')]:
            bad=copy.deepcopy(auth);mutate(bad)
            self.selection['authorization']=self.put(self.root/'bad-auth.json',bad)
            self.recipe['packet_selection']=self.put(self.root/'selection.json',self.selection)
            with self.assertRaises(ValueError):self.context()
        self.reauthorize();message=C.document(auth['source_message']);message['role']='assistant'
        source=self.put(self.root/'assistant-message.json',message);auth['source_message']=source
        self.selection['authorization']=self.put(self.root/'bad-auth.json',auth)
        self.recipe['packet_selection']=self.put(self.root/'selection.json',self.selection)
        with self.assertRaisesRegex(ValueError,'user message'):self.context()
    def test_excluded_full_capture_failure_is_still_rejected(self):
        value=C.document(self.full_ref);value['outcomes'][1]['full']['ok']=False
        changed=self.put(Path(self.full_ref['path']),value);self.recipe.update(input=changed,baseline=changed)
        with self.assertRaises(ValueError):self.context()
    def test_native_qualification_and_file_identity_before_dispatch(self):
        W.selected_native_profile(self.recipe,True)
        Path(self.native['path']).write_bytes(b'x'*self.native['bytes'])
        with self.assertRaisesRegex(ValueError,'digest'):W.selected_native_profile(self.recipe,True)
        Path(self.native['path']).write_bytes(b'not executable; metadata fixture')
        q=C.document(self.profile['qualification']);q['assertions']['unchanged_packet_values']=False
        self.profile['qualification']=self.put(self.root/'bad-qualification.json',q)
        self.recipe['native_execution_profile']=self.put(self.root/'native-profile.json',self.profile)
        with self.assertRaisesRegex(ValueError,'qualification'):W.selected_native_profile(self.recipe)
    def test_selected_command_surface_and_fixed_batch(self):
        ctx=self.context();key=[ctx['tag'],'r0','check',0];args=['check-paths',ctx['plan'],'r0','--limit','1000','--packets']
        C.validate_selected_command(ctx,key,args,self.config)
        for badkey,badargs in [(key,args[:-1]),(key,args[:4]+['1','--packets']),
            ([ctx['tag'],'r1','check',0],args[:2]+['r1']+args[3:]),([['packets',ctx['id']],'r0','check',0],args),
            ([ctx['tag'],'r0','choose'],['choose',ctx['plan'],'r0'])]:
            with self.assertRaises(ValueError):C.validate_selected_command(ctx,badkey,badargs,self.config)
    def test_first_transition_and_failure_or_fixed_drift_reject(self):
        W.admit_previous(self.recipe,self.context())
        for key,value in [('memory',{}),('temp_storage',{'changed':True})]:
            bad=copy.deepcopy(self.recipe);bad[key]=value
            with self.assertRaises(ValueError):W.admit_previous(bad,W.native_context(bad,C.context(bad),self.binding))
        self.old_result['status']='failed_or_unknown'
        old_ref=self.put(self.old_attempt/'result.json',self.old_result)
        review=C.document(self.recipe['previous']['review']);review['result']=old_ref
        self.recipe['previous']={'result':old_ref,'review':self.put(self.root/'failure-review.json',review)}
        with self.assertRaises(ValueError):W.admit_previous(self.recipe,self.context())
    def test_missing_authorization_does_not_create_attempt_or_change_pointer(self):
        self.selection.pop('authorization');self.recipe['packet_selection']=self.put(self.root/'selection.json',self.selection)
        before=(self.control/'current.json').read_bytes()
        with mock.patch.object(W,'selected_native_profile',wraps=W.selected_native_profile),mock.patch.object(W,'SUPERVISOR_SHA',self.recipe['code']['supervisor']['sha256']):
            with self.assertRaises((ValueError,KeyError)):W.run(self.recipe)
        self.assertEqual(before,(self.control/'current.json').read_bytes())
        self.assertFalse((self.attempt/'started.json').exists())
    def test_output_rejects_all_members_claim_or_wrong_namespace_scope(self):
        ctx=self.context();value={'automatic_selection':False,'migration_executed':False,'full_review_sha256':ctx['id'],
             'inventory_delta':self.stable(),'outcomes':[C.document(self.paths_ref)['outcomes'][i] for i in [0,2]],
             'outcome_counts':{'requested_members':2,'failures':0},'packet_selection':ctx['selection'],
             'paths_review':ctx['paths_input'],'native_execution_profile':self.recipe['native_execution_profile'],
             'excluded':ctx['excluded'],'scope':'user_selected_current_catalogs_external_packets',
             'prerequisite_members':3,'external_unselected_assessed':False}
        C.validate_output(value,ctx)
        for change in [lambda x:x.update(excluded=[]),lambda x:x.update(external_unselected_assessed=True),
                       lambda x:x['outcomes'].reverse(),lambda x:x.update(packet_selection=self.full_ref),
                       lambda x:x.update(native_execution_profile=self.full_ref)]:
            bad=copy.deepcopy(value);change(bad)
            with self.assertRaises(ValueError):C.validate_output(bad,ctx)

    def fake_frozen(self):
        class Interrupted(Exception):pass
        class Pause(Exception):pass
        return types.SimpleNamespace(revision=lambda m:(m.st_dev,m.st_ino,m.st_size,m.st_mtime_ns,m.st_ctime_ns),
                                     InterruptedOperation=Interrupted,PauseRequested=Pause)
    def test_binding_attribution_reset_failure_and_preflight(self):
        ctx=self.context();self.execution_files(); fixture=self;called=[]
        class Base:
            def __init__(self,root):self.root=root;self.binding=fixture.binding;self.config=fixture.config;self.fail=False;self.closed=False
            def close(self):self.closed=True
            def call(self,key,arguments):
                called.append((self.binding,self.binary))
                if self.fail:raise RuntimeError('synthetic launch failure')
                record={'sequence':2,'source_binding':self.binding,'argv':[str(self.binary),*[str(x) for x in arguments]]}
                directory=self.root/'commands/000000002';directory.mkdir();fixture.put(directory/'result.json',record)
                return {'record':record,'ok':True}
        klass=W.admitted_runner_type(Base,self.recipe,ctx,self.attempt,self.fake_frozen(),mock.Mock())
        runner=klass(self.run);args=['check-paths',ctx['plan'],'r0','--limit',1000,'--packets'];key=[ctx['tag'],'r0','check',0]
        bad=['check-paths',ctx['plan'],'r1','--limit',1000,'--packets']
        with self.assertRaises(ValueError):runner.call([ctx['tag'],'r1','check',0],bad)
        self.assertEqual(called,[])
        result=runner.call(key,args)
        self.assertEqual(result['record']['source_binding'],ctx['effective_binding'])
        self.assertEqual(result['record']['argv'][0],self.native['path'])
        self.assertIs(runner.binding,self.binding)
        runner.fail=True
        with self.assertRaisesRegex(RuntimeError,'launch failure'):runner.call(key,args)
        self.assertIs(runner.binding,self.binding)
        self.assertEqual(called[-1][0],ctx['effective_binding'])
    def test_actual_base_replay_and_pause_do_not_launch_or_reserve(self):
        R=load('selected_base_replay','run_lightroom_inspection.py');fixture=self;ctx=self.context()
        self.execution_files()
        class Base(R.Runner):
            def __init__(self,root):self.root=root;self.config=fixture.config;self.binding=fixture.binding;self.generation=None
            def close(self):pass
            def space(self,minimum=None):pass
        runner=W.admitted_runner_type(Base,self.recipe,ctx,self.attempt,R,mock.Mock())(self.run)
        key=[ctx['tag'],'r0','check',0];args=['check-paths',ctx['plan'],'r0','--limit','1000','--packets']
        directory=self.run/'commands/000000002';directory.mkdir();stdout=self.put(directory/'stdout',{'checked':0})
        record={'sequence':2,'source_binding':ctx['effective_binding'],'argv':[self.native['path'],*args],
                'key':key,'requested_arguments':args,'stdout':{'path':'commands/000000002/stdout','sha256':stdout['sha256']},
                'stdout_cap':1024,'exit_code':0,'failure':None,'log_errors':[]}
        self.put(directory/'result.json',record)
        self.put(self.run/'steps'/(C.sha(C.encoded(key))+'.json'),{'sequence':2,'record':'commands/000000002/result.json'})
        before=(self.run/'journal.json').read_bytes()
        with mock.patch.object(R.subprocess,'Popen',side_effect=AssertionError('must not launch')):
            self.assertEqual(runner.call(key,args)['value'],{'checked':0})
            self.put(self.run/'pause-request',{'owner':'synthetic-test'})
            with self.assertRaises(R.PauseRequested):runner.call([ctx['tag'],'r2','check',0],args[:2]+['r2']+args[3:])
        self.assertEqual(before,(self.run/'journal.json').read_bytes());self.assertIs(runner.binding,self.binding)
    def test_replaced_successor_is_rejected_by_actual_base_before_reservation(self):
        R=load('selected_base_stamp','run_lightroom_inspection.py');fixture=self;ctx=self.context();self.execution_files()
        class Base(R.Runner):
            def __init__(self,root):self.root=root;self.config=fixture.config;self.binding=fixture.binding;self.generation=None
            def close(self):pass
            def space(self,minimum=None):pass
        runner=W.admitted_runner_type(Base,self.recipe,ctx,self.attempt,R,mock.Mock())(self.run)
        before=(self.run/'journal.json').read_bytes();replacement=self.root/'replacement';replacement.write_bytes(Path(self.native['path']).read_bytes())
        os.replace(replacement,self.native['path'])
        with mock.patch.object(R.subprocess,'Popen',side_effect=AssertionError('must not launch')):
            with self.assertRaisesRegex(ValueError,'binary changed'):runner.call([ctx['tag'],'r0','check',0],['check-paths',ctx['plan'],'r0','--limit',1000,'--packets'])
        self.assertEqual(before,(self.run/'journal.json').read_bytes());self.assertIs(runner.binding,self.binding)
    def test_selected_phase_dispatches_only_choices_and_preserves_pause(self):
        ctx=self.context();frozen=self.fake_frozen();frozen.inventory_delta=lambda a,b:self.stable();frozen.unchanged_inventory=C.unchanged
        frozen.read_json=lambda p:json.loads(Path(p).read_bytes());fixture=self
        class Runner:
            def __init__(self):self.config=fixture.config;self.calls=[];self.paused=False
            def command_document(self,n,command):return {'synthetic':'inventory'}
            def require(self,key,args):
                C.validate_selected_command(ctx,key,[str(x) for x in args],self.config);self.calls.append((key,args))
                if self.paused and args[0]=='check-paths':raise frozen.PauseRequested('synthetic pause')
                value={'checked':0} if args[0]=='check-paths' else {'revision_id':args[2]} if args[0]=='report' else {'synthetic':'inventory'}
                return {'record':{'sequence':len(self.calls)+1},'value':value}
            def pages(self,plan,revision,name,key):return fixture.empty_page()
            def summary(self,name,value):
                p=fixture.run/'reports'/(name+'.json');fixture.put(p,value);return p
        runner=Runner();path=W.selected_path_phase(runner,self.recipe['input']['path'],self.recipe,ctx,frozen)
        self.assertEqual(path,Path(ctx['output']));C.validate_output(json.loads(path.read_bytes()),ctx)
        self.assertEqual([a[2] for _,a in runner.calls if a[0]=='check-paths'],['r0','r2'])
        # Existing completed output still gets a fresh inventory, then no packet work.
        replay=Runner();W.selected_path_phase(replay,self.recipe['input']['path'],self.recipe,ctx,frozen)
        self.assertEqual(len(replay.calls),1)
        path.unlink();paused=Runner();paused.paused=True
        with self.assertRaises(frozen.PauseRequested):W.selected_path_phase(paused,self.recipe['input']['path'],self.recipe,ctx,frozen)
        self.assertFalse(path.exists())
    def test_continuation_and_archival_validation_do_not_reopen_old_journal(self):
        self.execution_files();self.put(self.attempt/'process.json',{'pid':345})
        profile=C.document(self.recipe['canonical_hash_profile'])
        ep=self.put(self.attempt/'execution-profile.json',W.execution_profile_value(self.recipe,self.attempt))
        ec=self.put(self.attempt/'execution-profile-consumed.json',{'execution_profile':ep,'helper':profile['helper'],'pid':345})
        np=C.reference(self.attempt/'native-execution.json')
        nc=self.put(self.attempt/'native-execution-consumed.json',{'execution':np,'profile':self.recipe['native_execution_profile'],'pid':345})
        phase=self.put(self.run/'reports/phase-selected.json',{'phase':'packets','input':self.full_ref['path'],
                      'binding':self.binding,'status':'paused'})
        result={'status':'paused_at_command_boundary','root_reaped':True,'ownership_status':'observed_owned_processes_reaped',
                'cleanup':None,'new_command_failures':[],'exit_code':1,'journal':self.recipe['journal'],'next_command':2,
                'phase':phase,'phase_name':'packets','phase_input':self.full_ref,'execution_profile':ep,
                'execution_profile_consumed':ec,'native_execution':np,'native_execution_consumed':nc}
        rr=self.put(self.attempt/'result.json',result)
        review=self.put(self.root/'selected-review.json',{'status':'PASS','result':rr,'binding':self.recipe['binding'],
             'execution_profile':ep,'native_execution':np,'native_execution_consumed':nc,
             'packet_selection':self.recipe['packet_selection'],'native_execution_profile':self.recipe['native_execution_profile']})
        next_recipe=copy.deepcopy(self.recipe);next_recipe.pop('packet_transition');next_recipe['attempt_id']='00000000-0000-4000-8000-000000000003'
        next_recipe['previous']={'result':rr,'review':review}
        self.put(self.control/'current.json',{'attempt_id':self.recipe['attempt_id']})
        W.admit_previous(next_recipe,W.native_context(next_recipe,C.context(next_recipe),self.binding))
        self.put(self.run/'journal.json',{'next_command':99})  # Historical checkpoint is now deliberately stale.
        W.validate_selected_previous_history(next_recipe)
        for field in ['packet_selection','native_execution_profile']:
            bad=copy.deepcopy(next_recipe);bad[field]=self.full_ref
            with self.assertRaises((ValueError,KeyError)):W.validate_selected_previous_history(bad)
        removed=copy.deepcopy(next_recipe);removed.pop('packet_selection')
        with self.assertRaises(ValueError):W.validate_selected_previous_history(removed)
    def test_early_guard_protects_count_and_metadata_without_new_quota(self):
        monitor=mock.Mock();directory=self.run/'commands/000000002';directory.mkdir();(directory/'result.json').write_bytes(b'x')
        guard=W.NewCommandBudget(self.run,2,monitor);guard.bytes=12*W.MIB-1;guard.completed({'sequence':2})
        monitor.pause.assert_called_once();guard.completed({'sequence':2});monitor.pause.assert_called_once()
        other=mock.Mock();guard=W.NewCommandBudget(self.run,2-17999,other);guard.last=1;guard.completed({'sequence':2})
        other.pause.assert_called_once()

    def native_record(self, number, key, arguments, value, *, baseline=False):
        directory=self.run/'commands'/f'{number:09d}';directory.mkdir()
        stdout=directory/'stdout';stdout.write_bytes(C.encoded(value));stderr=directory/'stderr';stderr.write_bytes(b'')
        record={'sequence':number,'key':key,'requested_arguments':arguments,
                'argv':[str(self.run/'lightroom_inspect') if baseline else self.native['path'],*arguments],
                'capture_path':None,'started_unix':float(number),'finished_unix':number+.5,
                'source_binding':self.binding if baseline else self.context()['effective_binding'],
                'exit_code':0,'failure':None,'log_errors':[],'stdout_cap':8*1024**2,'stderr_cap':1024**2}
        for name,path in [('stdout',stdout),('stderr',stderr)]:
            record[name]={'path':str(path.relative_to(self.run)),'bytes':path.stat().st_size,'sha256':C.reference(path)['sha256']}
        self.put(directory/'result.json',record)
        self.put(directory/'started.json',{k:record[k] for k in ['sequence','key','requested_arguments','argv','capture_path','started_unix','source_binding']})
        self.put(directory/'process.json',{'pid':number+1200,'process_group':number+1200,'argv':record['argv'],'started_unix':number+.1})
        self.put(self.run/'steps'/(C.sha(C.encoded(key))+'.json'),{'sequence':number,'record':str((directory/'result.json').relative_to(self.run))})
        return record

    def audit_fixture(self, terminal):
        self.recipe['grant']=self.put(self.root/'synthetic-grant.json',{'status':'EXECUTION_GRANTED',
            'recipe_body_sha256':C.sha(C.encoded(self.recipe)),'scope':'packets','attempt_id':self.recipe['attempt_id']})
        ctx=self.context();self.execution_files();recipe_ref=C.reference(self.attempt/'recipe.json')
        self.put(self.attempt/'process.json',{'pid':345,'argv':[self.recipe['code']['python']['path'],'-I','-B',
            self.recipe['code']['controller']['path'],'--child',recipe_ref['path'],recipe_ref['sha256']]})
        ep=self.put(self.attempt/'execution-profile.json',W.execution_profile_value(self.recipe,self.attempt))
        ec=self.put(self.attempt/'execution-profile-consumed.json',{'execution_profile':ep,
            'helper':C.document(self.recipe['canonical_hash_profile'])['helper'],'pid':345})
        np=C.reference(self.attempt/'native-execution.json')
        nc=self.put(self.attempt/'native-execution-consumed.json',{'execution':np,'profile':self.recipe['native_execution_profile'],'pid':345})
        phase={'phase':'packets','input':self.full_ref['path'],'binding':self.binding,'status':'paused'}
        result={'status':'paused_at_command_boundary','root_reaped':True,'ownership_status':'observed_owned_processes_reaped',
                'cleanup':None,'new_command_failures':[],'exit_code':1,'phase_name':'packets','phase_input':self.full_ref,
                'execution_profile':ep,'execution_profile_consumed':ec,'native_execution':np,'native_execution_consumed':nc,'logs':{}}
        for name in ['stdout','stderr']:
            p=self.attempt/(name+'.log');p.write_bytes(b'')
            result['logs'][name]={'complete':True,'error':None,'truncated':False,'observed_bytes':0,'retained_bytes':0,'reference':C.reference(p)}
        number=2
        if terminal:
            inventory={'complete':True,'candidates':[x['candidate'] for x in C.document(self.main_ref)['outcomes']]}
            self.native_record(1,['full','baseline'],['discover',self.config['catalog_root']],inventory,baseline=True)
            self.native_record(number,[ctx['tag'],'a'*32,'discover-admission'],['discover',self.config['catalog_root']],inventory)
            first=number;number+=1
            rows=[]
            for index in [0,2]:
                rev=f'r{index}';self.native_record(number,[ctx['tag'],rev,'check',0],['check-paths',ctx['plan'],rev,'--limit','1000','--packets'],{'checked':0});number+=1
                for page in ['paths','packets','metadata-conflicts','issues']:
                    self.native_record(number,[ctx['tag'],rev,page,0],[page,ctx['plan'],rev,'--after','0','--limit','1000'],[]);number+=1
                report={'revision_id':rev,'counts':{}}
                self.native_record(number,[ctx['tag'],rev,'report'],['report',ctx['plan'],rev],report);number+=1
                rows.append({'key':self.keys[index],'revision':rev,'pages':{n:self.empty_page() for n in ['paths','packets','metadata-conflicts','issues']},'report':report})
            last=number;self.native_record(number,[ctx['tag'],'a'*32,'discover-end'],['discover',self.config['catalog_root']],inventory);number+=1
            self.native_record(number,[ctx['tag'],'families'],['families',ctx['plan']],{'families':self.families});number+=1
            output={'full_review_sha256':ctx['id'],'paths_review':self.paths_ref,'packet_selection':ctx['selection'],
                    'native_execution_profile':self.recipe['native_execution_profile'],'scope':'user_selected_current_catalogs_external_packets',
                    'prerequisite_members':3,'excluded':ctx['excluded'],'external_unselected_assessed':False,
                    'starting_inventory_command':first,'ending_inventory_command':last,'inventory_delta':self.stable(),
                    'outcome_counts':{'requested_members':2,'failures':0},'outcomes':rows,'families':{'families':self.families},
                    'automatic_selection':False,'migration_executed':False}
            result['output']=self.put(Path(ctx['output']),output)
            result.update(status='review_returned_not_acceptance',exit_code=0)
            phase.update(status='review_artifact_returned_not_acceptance',output=ctx['output'])
        else:result['pause']=self.put(self.run/'pause-request',{'owner':'synthetic held pause'})
        result['phase']=self.put(self.run/'reports/phase-audit.json',phase)
        result['journal']=self.put(self.run/'journal.json',{'next_command':number});result['next_command']=number
        self.put(self.control/'current.json',{'attempt_id':self.recipe['attempt_id']})
        self.audit_result=result
        request={'recipe':recipe_ref,'result':self.put(self.attempt/'result.json',result),
                 'terminal_wait':self.put(self.root/'terminal.json',{'exit_code':0,'output':''})}
        self.audit_request=request;self.rebind_audit_result();return request

    def rebind_audit_result(self):
        request=self.audit_request;request['result']=self.put(self.attempt/'result.json',self.audit_result)
        request['terminal_association']=self.put(self.root/'terminal-association.json',{'status':'PASS',
            'terminal_wait':request['terminal_wait'],'recipe':request['recipe'],'result':request['result'],
            'tool_session_id':'synthetic-session','reviewer':'synthetic independent owner reviewer'})

    def test_actual_auditor_terminal_and_paused_metadata_fixture(self):
        request=self.audit_fixture(True);proof=A.audit(W,C,request)
        self.assertEqual(proof['status'],'PASS');self.assertEqual(proof['next_command'],17)
        self.assertEqual(proof['packet_selection'],self.recipe['packet_selection'])
        self.assertIn('not S9 acceptance',proof['scope'])
        # The same executed body with a zero-command paused fixture is covered in
        # a separate fresh fixture so no recorded output is reused as a pause.
    def test_actual_auditor_zero_command_pause_and_foreign_terminal_reject(self):
        request=self.audit_fixture(False);self.assertEqual(A.audit(W,C,request)['status'],'PASS')
        association=C.document(request['terminal_association']);association['result']=self.full_ref
        request['terminal_association']=self.put(self.root/'wrong-terminal.json',association)
        with self.assertRaisesRegex(ValueError,'terminal association'):A.audit(W,C,request)
    def test_auditor_rejects_failed_owner_log_and_effective_native_substitution(self):
        request=self.audit_fixture(True)
        self.audit_result['logs']['stdout']['truncated']=True;self.rebind_audit_result()
        with self.assertRaisesRegex(ValueError,'log incomplete'):A.audit(W,C,request)
        self.audit_result['logs']['stdout']['truncated']=False;self.rebind_audit_result()
        p=self.run/'commands/000000002/result.json';v=json.loads(p.read_bytes());v['source_binding']=self.binding;self.put(p,v)
        with self.assertRaisesRegex(ValueError,'identity differs'):A.audit(W,C,request)
    def test_auditor_rejects_inventory_or_family_change_and_missing_zero(self):
        self.audit_fixture(True);ctx=self.context();out=C.document(self.audit_result['output'])
        # Mutations are made only to tiny fixture stdout, with its descriptor
        # rebound so these test semantics, rather than stale-digest rejection.
        def replace_stdout(number, value):
            directory=self.run/'commands'/f'{number:09d}';p=directory/'stdout';p.write_bytes(C.encoded(value))
            record=json.loads((directory/'result.json').read_bytes());record['stdout'].update(bytes=p.stat().st_size,sha256=C.reference(p)['sha256']);self.put(directory/'result.json',record)
        original=json.loads((self.run/'commands/000000015/stdout').read_bytes())
        replace_stdout(15,{'complete':False,'candidates':original['candidates']})
        with self.assertRaisesRegex(ValueError,'incomplete inventory'):A.validate_output_commands(W,C,self.recipe,ctx,self.binding,out,17)
        replace_stdout(15,original);changed=copy.deepcopy(out);changed['families']={'families':[]}
        with self.assertRaisesRegex(ValueError,'family evidence'):A.validate_output_commands(W,C,self.recipe,ctx,self.binding,changed,17)
        replace_stdout(3,{'checked':1})
        with self.assertRaises((ValueError,FileNotFoundError)):A.validate_output_commands(W,C,self.recipe,ctx,self.binding,out,17)
    def test_auditor_pending_states_and_count_mismatch_fail(self):
        self.audit_fixture(True);ctx=self.context();out=C.document(self.audit_result['output'])
        out['outcomes'][0]['pages']['paths'].update(rows=1,counts={'pending':1})
        with self.assertRaisesRegex(ValueError,'report/path states'):A.validate_output_commands(W,C,self.recipe,ctx,self.binding,out,17)
        out['outcomes'][0]['pages']['paths'].update(rows=2)
        with self.assertRaisesRegex(ValueError,'count reconciliation'):A.validate_output_commands(W,C,self.recipe,ctx,self.binding,out,17)
    def test_audit_budget_clamps_growth_read_and_restores_contract(self):
        original=C.raw;caps=[]
        def fake(path,cap):caps.append(cap);return b'x'*cap
        with mock.patch.object(C,'raw',fake):
            with A.metadata_budget(C,maximum=10):
                C.raw('synthetic',8);self.assertEqual(caps,[8])
                C.raw('synthetic',8);self.assertEqual(caps,[8,1])
                with self.assertRaises(ValueError):C.raw('synthetic',8)
            self.assertIs(C.raw,fake)
        self.assertIs(C.raw,original)



if __name__ == '__main__':unittest.main()
