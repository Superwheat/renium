export const settingsStoreTreeRuntime = `
function createSettingsStoreTree(options){
  var tree=null,expanded={},selectedId=null,byId={};
  var query='',searchCollapsed={},flat=[],sizer=null,rowsElement=null,paintQueued=false;
  var rowHeight=22,overscan=6,treeElement=options.treeElement,searchElement=options.searchElement;
  function escapeHtml(value){return String(value==null?'':value).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;')}
  function iconName(className){if(options.iconNames.has(className))return className;var fallback=className&&className.slice(-7)==='Service'?'Service':'Class';return options.iconNames.has(fallback)?fallback:className}
  function indexTree(){byId={};function walk(node){byId[node.settingsId]=node;if(node.children)for(var i=0;i<node.children.length;i++)walk(node.children[i])}var roots=(tree&&tree.roots)||[];for(var i=0;i<roots.length;i++)walk(roots[i])}
  function highlight(name){
    var text=String(name==null?'':name);
    if(!query)return escapeHtml(text);
    var index=text.toLowerCase().indexOf(query);
    if(index<0)return escapeHtml(text);
    return escapeHtml(text.slice(0,index))+'<span class="rbhi">'+escapeHtml(text.slice(index,index+query.length))+'</span>'+escapeHtml(text.slice(index+query.length));
  }
  function rowHtml(item){
    var node=item.node;
    var html='<div class="row'+(selectedId===node.settingsId?' selected':'')+(item.match?' rbmatch':'')+'" data-store-id="'+escapeHtml(node.settingsId)+'" style="padding-left:'+(item.depth*12+(options.rowPadding||0))+'px">';
    html+='<span class="twisty '+(item.hasChildren?(item.open?'open':''):'leaf')+'"></span>';
    html+='<img class="icon" src="'+options.assetBase+'/'+escapeHtml(iconName(node.className))+'.png">';
    html+='<span class="labelWrap"><span class="name">'+(item.match?highlight(node.name):escapeHtml(node.name))+'</span></span></div>';
    return html;
  }
  function buildFlat(){
    flat=[];
    if(!tree)return;
    var roots=tree.roots||[];
    if(!query){
      (function walk(list,depth){
        for(var i=0;i<list.length;i++){
          var node=list[i],children=node.children||[],open=!!expanded[node.settingsId];
          flat.push({node:node,depth:depth,hasChildren:children.length>0,open:open,match:false});
          if(children.length&&open)walk(children,depth+1);
        }
      })(roots,0);
      return;
    }
    var included={};
    function mark(node){
      var children=node.children||[],any=false;
      for(var i=0;i<children.length;i++)if(mark(children[i]))any=true;
      var self=(String(node.name)+' '+String(node.className)).toLowerCase().indexOf(query)>=0;
      if(self||any){included[node.settingsId]=self?2:1;return true}
      return false;
    }
    for(var i=0;i<roots.length;i++)mark(roots[i]);
    (function walk(list,depth){
      for(var i=0;i<list.length;i++){
        var node=list[i],flag=included[node.settingsId];
        if(!flag)continue;
        var children=node.children||[],open=!searchCollapsed[node.settingsId];
        flat.push({node:node,depth:depth,hasChildren:children.length>0,open:open,match:flag===2});
        if(children.length&&open)walk(children,depth+1);
      }
    })(roots,0);
  }
  function ensureShell(){
    if(sizer&&sizer.parentNode===treeElement)return;
    treeElement.innerHTML='';
    sizer=document.createElement('div');sizer.className='rbSizer';
    rowsElement=document.createElement('div');rowsElement.className='rbRows';
    sizer.appendChild(rowsElement);treeElement.appendChild(sizer);
  }
  function paint(){
    if(!tree||!flat.length)return;
    ensureShell();
    sizer.style.height=(flat.length*rowHeight)+'px';
    var viewportHeight=treeElement.clientHeight||options.fallbackHeight||300,scrollTop=treeElement.scrollTop;
    var start=Math.max(0,Math.floor(scrollTop/rowHeight)-overscan);
    var end=Math.min(flat.length,Math.ceil((scrollTop+viewportHeight)/rowHeight)+overscan);
    var output=[];for(var i=start;i<end;i++)output.push(rowHtml(flat[i]));
    rowsElement.style.transform='translateY('+(start*rowHeight)+'px)';
    rowsElement.innerHTML=output.join('');
  }
  function schedulePaint(){
    if(paintQueued)return;
    paintQueued=true;
    requestAnimationFrame(function(){paintQueued=false;paint()});
  }
  function render(){
    if(!tree||!((tree.roots||[]).length)){sizer=null;rowsElement=null;treeElement.innerHTML=options.emptyHtml;return}
    buildFlat();
    if(!flat.length){sizer=null;rowsElement=null;treeElement.innerHTML='<div class="'+options.emptyClass+'">No matches.</div>';return}
    paint();
  }
  function select(id){
    selectedId=id;var node=byId[id];paint();
    if(node)options.onSelect(node);
  }
  function setTree(value){
    tree=value;expanded={};selectedId=null;query='';searchCollapsed={};searchElement.value='';treeElement.scrollTop=0;indexTree();
    var large=tree&&tree.instanceCount>800;
    var roots=(tree&&tree.roots)||[];
    (function walk(list,depth){for(var i=0;i<list.length;i++){var node=list[i];if(node.children&&node.children.length&&(!large||depth===0))expanded[node.settingsId]=true;if(node.children)walk(node.children,depth+1)}})(roots,0);
    selectedId=roots.length?roots[0].settingsId:null;
    searchElement.placeholder=tree?('Search '+tree.instanceCount+' instances'):'Search';
    render();
    if(selectedId)options.onSelect(byId[selectedId]);
  }
  function setError(message){
    tree=null;selectedId=null;sizer=null;rowsElement=null;searchElement.placeholder='Search';treeElement.innerHTML='';
    var element=document.createElement('div');element.className=options.errorClass;element.textContent=message;treeElement.appendChild(element);
  }
  treeElement.addEventListener('scroll',schedulePaint);
  treeElement.addEventListener('click',function(event){
    var row=event.target.closest('.row');if(!row)return;
    var id=row.dataset.storeId,node=byId[id];if(!node)return;
    var twisty=event.target.closest('.twisty');
    if(twisty&&!twisty.classList.contains('leaf')){
      if(query)searchCollapsed[id]=!searchCollapsed[id];else expanded[id]=!expanded[id];
      render();return;
    }
    select(id);
  });
  searchElement.addEventListener('input',function(){
    if(!tree)return;
    var next=(searchElement.value||'').trim().toLowerCase();
    if(next===query)return;
    query=next;searchCollapsed={};treeElement.scrollTop=0;render();
  });
  window.addEventListener('resize',function(){if(!options.isVisible||options.isVisible())schedulePaint()});
  return {setTree:setTree,setError:setError,paint:schedulePaint};
}
`;
