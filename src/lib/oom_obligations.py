"""Independent OOM obligations with compatible legacy kind/origin projections."""
import copy

PRIORITY={'unknown':0,'container':0,'runner':1,'builder':2,'host':3}


def obligations_for(state,key):
    """Read without mutation so identity/evidence guards can hold atomically."""
    obligations=copy.deepcopy(state.get('pending_oom_obligations',{}).get(key,{}))
    legacy=state.get('pending_oom',{}).get(key)
    if legacy is not None and legacy not in obligations:
        origin=copy.deepcopy(state.get('pending_oom_origins',{}).get(key,{}))
        if origin.get('kind')!=legacy:
            origin=dict(kind=legacy,boot_id=state.get('host_health',{}).get(key,{}).get('boot_id'),
                        event_ids=[],quarantined=True)
        obligations[legacy]=origin
    return obligations


def pending_kinds(state,key):
    return set(obligations_for(state,key))


def sync_pending(state):
    hosts=set(state.get('pending_oom',{}))|set(state.get('pending_oom_obligations',{}))
    ledger={key:rows for key in hosts if (rows:=obligations_for(state,key))}
    state['pending_oom_obligations']=ledger
    state['pending_oom']={key:max(rows,key=lambda kind:PRIORITY.get(kind,0)) for key,rows in ledger.items()}
    state['pending_oom_origins']={key:copy.deepcopy(ledger[key][kind])
                                  for key,kind in state['pending_oom'].items()}
    return ledger


def remember_oom(state,key,kind,boot_id,event_ids):
    ledger=sync_pending(state)
    rows=ledger.setdefault(key,{})
    origin=rows.setdefault(kind,dict(kind=kind,boot_id=boot_id,event_ids=[],quarantined=not bool(boot_id)))
    origin['event_ids']=sorted(set(origin.get('event_ids',[]))|set(event_ids))
    # Preserve the first originating boot, including quarantined legacy flags.
    sync_pending(state)


def clear_oom(state,key,kind=None):
    ledger=sync_pending(state)
    if kind is None:ledger.pop(key,None)
    elif key in ledger:
        ledger[key].pop(kind,None)
        if not ledger[key]:ledger.pop(key)
    # Remove the old projection before synchronizing, so it cannot resurrect
    # an explicitly completed obligation during the legacy import.
    state['pending_oom'].pop(key,None)
    state['pending_oom_origins'].pop(key,None)
    sync_pending(state)
