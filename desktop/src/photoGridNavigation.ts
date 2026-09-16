type FocusCandidate={dataset:{image?:string};focus:(options?:FocusOptions)=>void};

export function createKeyboardFocusRequest(){
  let requested:string|null=null;
  return {
    request:(image:string)=>{requested=image;},
    cancel:()=>{requested=null;},
    matches:(selected:string|null)=>requested!==null&&requested===selected,
    settle:(selected:string|null,candidates:Iterable<FocusCandidate>)=>{
      if(requested===null||requested!==selected)return false;
      const button=Array.from(candidates).find(value=>value.dataset.image===requested);if(!button)return false;
      button.focus({preventScroll:true});requested=null;return true;
    },
  };
}
export const focusLeavesGrid=(container:Pick<Node,'contains'>,next:Node|null)=>!container.contains(next);
