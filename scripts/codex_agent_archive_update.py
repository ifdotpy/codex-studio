"""Apply scoped archive checks without a backend or native restart."""
import hashlib
from pathlib import Path
import sys
from codex_source import signature, source_function

SOURCE_HASH = '52af1971324ec11cb8fbd6772296e3ee34a6ce71a15f74b0400c32b4438123b0'
BEFORE = {
    '_blockers': '862cece54b0669fecdae3207dd264b49a1cf69acac518c7b1e708010e78075b4',
    'manage_agent': '4422dea9e9c5c3f43b55d68a4f49a7439d0373dcebb8e78320a532691f30d461',
}
AFTER = {
    '_blockers': '6fdb672fbfb094127cc7eeb3fed6e1eaf9dec7d9229c9519a515780cc91a0843',
    'manage_agent': 'b1516158a2b9446a6427fdf2060b247cebd7d7015a20c630c1b8b39b4055240f',
    '_archive_record': '696a072b68b07fb665a0854283866898017bf8eb2f2e48251e4b03e9d0140123',
}
DEPENDENCIES = {
    'management_tools': '3f0e5752d349841905ade7dd7a6397dd436044cc52ab991a934b9c538e528ec5',
    '_authorize': '6b404412549e78b696ce6800d4b32f953e7dd6a4f0e3a0337e560a8dc140e211',
    '_brief': '554d25e71966569354aad2de80b02067a78880bfa6f7c51a702e5198106e578c',
    '_completed_native_turn': '1b0d746698c9a30b82b63c938e6f793ae32e7b708be3b6fb6139ed9da479b47a',
    '_missing_transferred_history': 'f26742202e51dbec51a8bdf60ae21d6cfa895a842c4a683577b2b7f76c6aae54',
    '_git': '98e2dd9effc8b1ef860e8bcd9307e45fa3afd47a1332f832c0e6c94391d651be',
    '_worktree_entries': 'f9163579bcc0c3ddf4c1a7354dc40c44ec01c2de8f8ad87b2fbb7c798481e87d',
    '_worktree_registration': '6fd42b60eb784ce7575f87a8a184a236878780933590a03d49006ca3919722e5',
    '_branch_commit': 'd76c9d31e03d9bb1efd3ff92f7fb78b961e9d5a560d3f1ac7e939ba80cc8926f',
    '_save_archive_ref': '107268bf204c56653fb86ea73eab5c5f775daf6185c33b6636dc43a68e5169f0',
    '_prune_missing_registration': '2c0322702613e287703aaaa099161e9c4bba81ef3de337ac77800a3aedb65916',
    '_worker_worktree_root': '596bf392aa2234b653ee2a05e2b51927f0b4b92d0eefef9417495ba873f8f452',
    '_worktree_check': '667faa3020013dc445a0d7210943feb77429aa2921f3afd7be2c717a43b22425',
    '_cleanup_worktree': '45d61c4decf29e47abcb95ae16e3c9237e50bce235f91a5fdd653eda56c31897',
    '_finished': '109e5dca2965d31a3fe15306a89b83a59977ecbe74b0d61bf2318ef331f9f0f0',
    'parked_after_turn': 'a929b262b357435646b146479fdb29ec26c9d849bbbaa8220015690fce8d33b4',
    '_park_actor': '7f78625fad580e8ee1ee70c62a396fe11df3769f69699bf8270b570930158703',
    '_park_target': '40f30cd6fa5a917b52a25e9d477527370ac305a012aa2a9a8552ce88c5b7b3aa',
    '_event_name': '656bd17011604c605a7112bcacadc5fadc658477ecf7e61d53d919ba842debfe',
    '_park_action': '6ce477d9adf9ed11f94aeaf81c8f81984e5aaf1970fbd7e82420834c9287991f',
    'reviewer_result_delivered': 'ad8a9b4c1422dab04c382917e22542df61650a9c2db4da02392c90dc65712b35',
    '_archive_reviewer': '389ffbf339c247832f1913bba9d9b567ad70d651ddc34e2f569afd99a6b90c2a',
    '_archive_finished': '75b37464af46f2974a6fdfcef8c06fff923f6588ef0b365ca87f402ce442d3ed',
    'worktree_maintenance_report': '63e511046a75907ac5fa56de73f114d32083e95c83dcb72df3a6c8c50c675f0a',
    '_restore_worktree': '75f2fcf631d550c109ee8c7d77506a19e358dbef92f28c1ac59eeb0017d51b71',
}


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    management = sys.modules.get('codex_agent_management')
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or management is None or Path(management.__file__).resolve().parent != scripts):
        raise RuntimeError('The running backend source identity differs')
    path = scripts / 'codex_agent_management.py'
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_HASH:
        raise RuntimeError('The reviewed archive source differs')
    desired = {name: source_function(raw, [name], vars(management), str(path))[0]
               for name in AFTER}
    if any(signature(function) != AFTER[name] for name, function in desired.items()):
        raise RuntimeError('The reviewed archive functions differ')
    with runtime.lock:
        for name, expected in DEPENDENCIES.items():
            if signature(getattr(management, name, None)) != expected:
                raise RuntimeError('The running archive guard differs: ' + name)
        for name, before in BEFORE.items():
            if signature(getattr(management, name, None)) not in {before, AFTER[name]}:
                raise RuntimeError('The running archive function differs: ' + name)
        helper = getattr(management, '_archive_record', None)
        if helper is not None and signature(helper) != AFTER['_archive_record']:
            raise RuntimeError('The running archive helper differs')
        if (helper is not None and all(signature(getattr(management, name)) == AFTER[name]
                                      for name in BEFORE)):
            return {'status': 'already_applied'}
        management._archive_record = desired['_archive_record']
        for name in BEFORE:
            current, replacement = getattr(management, name), desired[name]
            current.__code__ = replacement.__code__
            current.__defaults__ = replacement.__defaults__
            current.__kwdefaults__ = replacement.__kwdefaults__
    return {'status': 'applied'}
