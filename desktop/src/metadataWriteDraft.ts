import type { Edit } from './metadataWrite';

export type EditForm='set'|'remove'|'append'|'localized';
export function buildMetadataEdit(form:EditForm, namespace:string, path:string, value:string, ordered:boolean, language:string):Edit {
  if(!namespace.trim()||!path.trim())throw new Error('Namespace and property path are required.');
  if(form==='set')return {operation:'set',namespace,path,value};
  if(form==='remove')return {operation:'remove',namespace,path};
  if(form==='append')return {operation:'append',namespace,path,value,ordered};
  if(!language.trim())throw new Error('Localized edits require a language.');
  return {operation:'localized',namespace,path,language,value};
}

export function encodeEditDocument(edits:Edit[]):Uint8Array {
  if(!edits.length||edits.length>1000)throw new Error('Metadata edit count must be 1 through 1000.');
  return new TextEncoder().encode(JSON.stringify(edits));
}
