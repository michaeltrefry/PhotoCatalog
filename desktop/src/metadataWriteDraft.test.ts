import { describe, expect, it } from 'vitest';
import { buildMetadataEdit, encodeEditDocument } from './metadataWriteDraft';

describe('metadata write drafts',()=>{
  it('maps every supported edit form without dropping form-specific authority',()=>{
    expect(buildMetadataEdit('set','urn:test','a/b','7',false,'')).toEqual({operation:'set',namespace:'urn:test',path:'a/b',value:'7'});
    expect(buildMetadataEdit('remove','urn:test','a/b','ignored',false,'')).toEqual({operation:'remove',namespace:'urn:test',path:'a/b'});
    expect(buildMetadataEdit('append','urn:test','items','x',true,'')).toEqual({operation:'append',namespace:'urn:test',path:'items',value:'x',ordered:true});
    expect(buildMetadataEdit('localized','urn:test','title','Bonjour',false,'fr-FR')).toEqual({operation:'localized',namespace:'urn:test',path:'title',language:'fr-FR',value:'Bonjour'});
  });
  it('encodes one exact UTF-8 document and rejects empty work',()=>{
    const edit=buildMetadataEdit('localized','urn:test','title','雪',false,'ja-JP');
    expect(new TextDecoder().decode(encodeEditDocument([edit]))).toBe(JSON.stringify([edit]));
    expect(()=>encodeEditDocument([])).toThrow(/1 through 1000/);
  });
});
