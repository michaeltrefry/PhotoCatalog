import type { useLightroomMigration } from '../state/useLightroomMigration';
import './lightroom.css';

type Controller=ReturnType<typeof useLightroomMigration>;
export function LightroomMigrationActivity({controller,onOpen}:{controller:Controller;onOpen:()=>void}){
  const value=controller.snapshot;
  if(!value&&!controller.outcomeUnknown&&!controller.stale&&!controller.statusError)return null;
  return <div className="activity lightroom-activity" role={controller.stale||controller.outcomeUnknown?'alert':'status'}>
    <span>{value?`Lightroom migration: ${value.phase.replaceAll('_',' ')}${value.progress?` · ${value.progress[0]} ${value.progress[1]}${value.progress[2]?` of ${value.progress[2]}`:''}`:''}`:controller.stale?'The retained migration guard is stale; backend ownership is unknown.':'Migration command acknowledgement is unknown; guarded status will reconcile it.'}</span>
    <button onClick={onOpen}>Review migration…</button>
  </div>;
}
