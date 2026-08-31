//! Raw artifact response policy.
//!
//! Artifact bytes are untrusted, agent-authored content. Every response built here receives the
//! exact CSP sandbox and egress policy from `lib/artifact-http.js`, including non-HTML bundle assets
//! and downloads.
//! The sandbox deliberately omits `allow-same-origin` so direct navigation cannot regain the
//! application's origin.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};

use crate::{error::AppError, model::ArtifactFile};

/// Exact document policy fixed by ADR-0003 and `lib/artifact-http.js:3`.
pub const DOCUMENT_SANDBOX: &str = "sandbox allow-scripts allow-popups allow-forms allow-modals; default-src 'none'; connect-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: blob: https://fonts.gstatic.com; img-src 'self' data: blob:; media-src 'self' data: blob:; worker-src 'self' blob:";

/// Stable marker used to prove that anchor mode adds exactly one server-owned bridge.
pub const ANCHOR_BRIDGE_MARKER: &str = "artifact-anchor-bridge";

/// Server-owned anchor bridge copied byte-for-byte from `lib/artifact-http.js`.
///
/// Keep this constant literal: artifact content must never be interpolated into the script. The
/// only variable portion is the optional page path, which [`inject_anchor_bridge`] JSON-encodes and
/// escapes before replacing the fixed `page=null` initializer.
pub const ANCHOR_BRIDGE: &str = r###"<script id="artifact-anchor-bridge">(function(){try{
"use strict";var d=document,w=window,page=null,picking=false,anchors=[],geometry=new Map(),drag=null,selection=null,threshold=4,preview=null,lastCandidate=null,pendingCandidate=null,candidateQueued=false;
var MEANINGFUL_TAGS=new Set(["h1","h2","h3","h4","h5","h6","p","li","dt","dd","tr","th","td","caption","section","article","aside","main","nav","header","footer","figure","figcaption","blockquote","pre","code","form","fieldset","details","summary","img","video","audio","canvas","svg"]);
var EXCLUDED_TAGS=new Set(["html","body","script","style","noscript"]);
function pageForLocation(){try{var match=w.location.pathname.match(/^(.*\/raw\/[^/]+\/)(.*)$/);if(!match)return null;return match[2].split("/").map(function(part){try{return decodeURIComponent(part);}catch(_){return part;}}).join("/")||null;}catch(_){return null;}}
if(page===null)page=pageForLocation();
function clamp(n){n=Number(n);return Number.isFinite(n)?Math.max(0,Math.min(1,n)):null;}
function dimensions(){var h=d.documentElement,b=d.body||{};return {w:Math.max(h.scrollWidth,h.clientWidth,b.scrollWidth||0,b.clientWidth||0,1),h:Math.max(h.scrollHeight,h.clientHeight,b.scrollHeight||0,b.clientHeight||0,1)};}
function post(message){try{w.parent.postMessage(message,"*");}catch(_){}}
function pathFor(el){try{var bits=[],node=el,depth=0;while(node&&node.nodeType===1&&depth++<8){var tag=node.tagName.toLowerCase(),i=1,prev=node;while((prev=prev.previousElementSibling))i++;bits.unshift(tag+":nth-child("+i+")");if(node===d.documentElement)break;node=node.parentElement;}return bits.join(">");}catch(_){return "";}}
function elementFor(ev){try{return ev.target&&ev.target.nodeType===1?ev.target:ev.target&&ev.target.parentElement||null;}catch(_){return null;}}
function cap(value,limit){try{return Array.from(value).slice(0,limit).join("");}catch(_){return "";}}
function clean(value){try{return typeof value==="string"?value.replace(/[\u0000-\u001F\u007F-\u009F]/g,"").trim():"";}catch(_){return "";}}
function normalizeNodeId(value){var cleaned=clean(value);return cleaned?cap(cleaned,128):null;}
function normalizeText(value){try{var collapsed=typeof value==="string"?value.replace(/\s+/g," ").trim():"";return collapsed?cap(collapsed,240):null;}catch(_){return null;}}
function normalizeQuote(el){try{var text=normalizeText(typeof el.innerText==="string"?el.innerText:el.textContent);if(text)return text;var names=["aria-label","alt","title"];for(var i=0;i<names.length;i++){var value=normalizeText(el.getAttribute&&el.getAttribute(names[i]));if(value)return value;}return null;}catch(_){return null;}}
function isValidCandidate(el){try{if(!el||el.nodeType!==1||!el.tagName)return false;var tag=String(el.tagName).toLowerCase();if(EXCLUDED_TAGS.has(tag))return false;if(el.hasAttribute&&(el.hasAttribute("data-artifact-anchor-selection")||el.hasAttribute("data-artifact-anchor-preview")))return false;var rect=el.getBoundingClientRect&&el.getBoundingClientRect();return !!rect&&Number.isFinite(Number(rect.width))&&Number.isFinite(Number(rect.height))&&Number(rect.width)>0&&Number(rect.height)>0;}catch(_){return false;}}
function findCandidate(el){try{var node=el,meaningful=null,fallback=null;while(node&&node.nodeType===1){if(isValidCandidate(node)){if(!fallback)fallback=node;var nodeId=normalizeNodeId(node.getAttribute&&node.getAttribute("data-artifact-node"));if(nodeId)return node;if(!meaningful&&MEANINGFUL_TAGS.has(String(node.tagName).toLowerCase()))meaningful=node;}node=node.parentElement;}return meaningful||fallback;}catch(_){return null;}}
function buildEnvelope(el,kind,approx){try{if(!isValidCandidate(el))return null;var rect=el.getBoundingClientRect(),size=dimensions(),left=Number(rect.left)+Number(w.scrollX||0),top=Number(rect.top)+Number(w.scrollY||0),rw=Number(rect.width),rh=Number(rect.height),x=clamp(left/size.w),y=clamp(top/size.h);if(x===null||y===null)return null;var bw=Math.min(clamp(rw/size.w),1-x),bh=Math.min(clamp(rh/size.h),1-y);if(!Number.isFinite(bw)||!Number.isFinite(bh)||bw<=0||bh<=0)return null;return {version:2,kind:kind,path:pathFor(el),nodeId:normalizeNodeId(el.getAttribute&&el.getAttribute("data-artifact-node")),quote:normalizeQuote(el),x:x,y:y,w:bw,h:bh,page:page,approx:approx};}catch(_){return null;}}
function clearPreview(){try{if(preview){preview.remove();preview=null;}}catch(_){preview=null;}}
function showPreview(el){try{clearPreview();if(!el)return;var rect=el.getBoundingClientRect?el.getBoundingClientRect():null;if(!rect)return;preview=d.createElement("div");preview.setAttribute("data-artifact-anchor-preview","");preview.style.cssText="position:fixed;z-index:2147483647;pointer-events:none;border:2px solid #0066cc;background:rgba(0,102,204,.1);box-sizing:border-box;";preview.style.left=(rect.left-2)+"px";preview.style.top=(rect.top-2)+"px";preview.style.width=(rect.width+4)+"px";preview.style.height=(rect.height+4)+"px";(d.body||d.documentElement).appendChild(preview);}catch(_){clearPreview();}}
function scheduleCandidate(next){try{pendingCandidate=next;if(candidateQueued)return;candidateQueued=true;var schedule=typeof w.requestAnimationFrame==="function"?w.requestAnimationFrame.bind(w):function(callback){callback();};schedule(function(){candidateQueued=false;var candidate=picking?pendingCandidate:null,envelope=candidate?buildEnvelope(candidate,"element",false):null;if(candidate&&!envelope)candidate=null;if(candidate===lastCandidate)return;lastCandidate=candidate;clearPreview();if(candidate)showPreview(candidate);post({type:"anchor:candidate",anchor:candidate?envelope:null});});}catch(_){candidateQueued=false;clearPreview();}}
function clearCandidate(){pendingCandidate=null;clearPreview();scheduleCandidate(null);}
function clearSelection(){try{if(selection)selection.remove();selection=null;}catch(_){selection=null;}}
function showSelection(a,b){try{if(!selection){selection=d.createElement("div");selection.setAttribute("data-artifact-anchor-selection","");selection.style.cssText="position:fixed;z-index:2147483647;pointer-events:none;border:2px solid #a66a2c;background:rgba(166,106,44,.14);box-sizing:border-box;";(d.body||d.documentElement).appendChild(selection);}var left=Math.min(a.x,b.x),top=Math.min(a.y,b.y);selection.style.left=left+"px";selection.style.top=top+"px";selection.style.width=Math.abs(a.x-b.x)+"px";selection.style.height=Math.abs(a.y-b.y)+"px";}catch(_){}}
function stopPicking(){picking=false;drag=null;clearSelection();clearCandidate();d.removeEventListener("pointerdown",down,true);d.removeEventListener("pointermove",move,true);d.removeEventListener("pointerup",up,true);d.removeEventListener("pointercancel",cancel,true);}
function position(anchor){try{var id=String(anchor&&anchor.id||""),path=anchor&&typeof anchor.path==="string"?anchor.path:"",x=clamp(anchor&&anchor.x),y=clamp(anchor&&anchor.y),bw=clamp(anchor&&anchor.w),bh=clamp(anchor&&anchor.h),box=bw!==null&&bh!==null&&bw>0&&bh>0,size=dimensions();if(x===null||y===null)return {id:id,lost:true};if(path){var el;try{el=d.querySelector(path);}catch(_){return {id:id,lost:true};}if(!el||typeof el.getBoundingClientRect!=="function")return {id:id,lost:true};var rect=el.getBoundingClientRect(),left=Number(rect.left),top=Number(rect.top),rw=Number(rect.width),rh=Number(rect.height);if(!Number.isFinite(left)||!Number.isFinite(top)||!Number.isFinite(rw)||!Number.isFinite(rh))return {id:id,lost:true};var state=geometry.get(id);if(!state||state.path!==path||state.x!==x||state.y!==y||state.bw!==bw||state.bh!==bh){var ox=x*size.w-(left+w.scrollX),oy=y*size.h-(top+w.scrollY);state={path:path,x:x,y:y,bw:bw,bh:bh,rx:rw?ox/rw:null,ry:rh?oy/rh:null,ox:ox,oy:oy,sw:box&&rw?Math.min(bw,1-x)*size.w/rw:null,sh:box&&rh?Math.min(bh,1-y)*size.h/rh:null,pw:box?Math.min(bw,1-x)*size.w:null,ph:box?Math.min(bh,1-y)*size.h:null};geometry.set(id,state);}var tx=left+(state.rx===null?state.ox:state.rx*rw),ty=top+(state.ry===null?state.oy:state.ry*rh);if(!Number.isFinite(tx)||!Number.isFinite(ty))return {id:id,lost:true};if(box){var tw=state.sw===null?state.pw:state.sw*rw,th=state.sh===null?state.ph:state.sh*rh;if(!Number.isFinite(tw)||!Number.isFinite(th)||tw<=0||th<=0)return {id:id,lost:true};return {id:id,x:tx,y:ty,w:tw,h:th,lost:false};}return {id:id,x:tx,y:ty,lost:false};}if(box){var bx=x*size.w-w.scrollX,by=y*size.h-w.scrollY,pw=Math.min(bw,1-x)*size.w,ph=Math.min(bh,1-y)*size.h;if(!Number.isFinite(bx)||!Number.isFinite(by)||!Number.isFinite(pw)||!Number.isFinite(ph)||pw<=0||ph<=0)return {id:id,lost:true};return {id:id,x:bx,y:by,w:pw,h:ph,lost:false};}var px=x*size.w-w.scrollX,py=y*size.h-w.scrollY;if(!Number.isFinite(px)||!Number.isFinite(py))return {id:id,lost:true};return {id:id,x:px,y:py,lost:false};}catch(_){return {id:String(anchor&&anchor.id||""),lost:true};}}
function retainGeometry(){try{var next=new Map();anchors.forEach(function(anchor){var id=String(anchor&&anchor.id||""),state=geometry.get(id);if(state)next.set(id,state);});geometry=next;}catch(_){geometry=new Map();}}
function repaint(){try{post({type:"anchor:positions",anchors:anchors.map(position)});}catch(_){}}
function down(ev){try{if(!picking||ev.button!==undefined&&ev.button!==0)return;var candidate=findCandidate(elementFor(ev)),envelope=candidate?buildEnvelope(candidate,"element",false):null;if(!candidate||!envelope){clearCandidate();return;}ev.preventDefault();ev.stopPropagation();drag={id:ev.pointerId,x:ev.clientX,y:ev.clientY,candidate:candidate,envelope:envelope,moved:false};scheduleCandidate(candidate);}catch(_){clearCandidate();}}
function move(ev){try{if(!picking)return;if(!drag){scheduleCandidate(findCandidate(elementFor(ev)));return;}if(ev.pointerId!==drag.id)return;ev.preventDefault();ev.stopPropagation();if(drag.moved||Math.abs(ev.clientX-drag.x)>threshold||Math.abs(ev.clientY-drag.y)>threshold){drag.moved=true;clearCandidate();showSelection({x:drag.x,y:drag.y},{x:ev.clientX,y:ev.clientY});}else scheduleCandidate(drag.candidate);}catch(_){clearCandidate();}}
function up(ev){try{if(!drag||ev.pointerId!==drag.id)return;ev.preventDefault();ev.stopPropagation();var current=drag,size=dimensions(),candidate=current.candidate,envelope=current.moved?current.envelope:buildEnvelope(candidate,"element",false);if(!envelope){stopPicking();return;}if(current.moved){var left=Math.min(current.x,ev.clientX)+Number(w.scrollX||0),top=Math.min(current.y,ev.clientY)+Number(w.scrollY||0),bx=clamp(left/size.w),by=clamp(top/size.h),bw=Math.min(Math.abs(ev.clientX-current.x)/size.w,1-(bx===null?1:bx)),bh=Math.min(Math.abs(ev.clientY-current.y)/size.h,1-(by===null?1:by));if(bx!==null&&by!==null&&Number.isFinite(bw)&&Number.isFinite(bh)&&bw>0&&bh>0)post({type:"anchor:picked",version:2,kind:"region",path:envelope.path,nodeId:envelope.nodeId,quote:envelope.quote,x:bx,y:by,w:bw,h:bh,page:page,approx:true});}else post({type:"anchor:picked",version:2,kind:"element",path:envelope.path,nodeId:envelope.nodeId,quote:envelope.quote,x:envelope.x,y:envelope.y,w:envelope.w,h:envelope.h,page:page,approx:false});stopPicking();}catch(_){stopPicking();}}
function cancel(ev){try{if(drag&&ev.pointerId===drag.id)stopPicking();}catch(_){stopPicking();}}
function receive(ev){try{if(ev.source!==w.parent||!ev.data||typeof ev.data!=="object")return;var type=ev.data.type;if(type==="anchor:pick-on"){if(!picking){picking=true;d.addEventListener("pointerdown",down,true);d.addEventListener("pointermove",move,true);d.addEventListener("pointerup",up,true);d.addEventListener("pointercancel",cancel,true);}}else if(type==="anchor:pick-off"){stopPicking();}else if(type==="anchor:repaint"){anchors=Array.isArray(ev.data.anchors)?ev.data.anchors.slice(0,200):[];retainGeometry();repaint();}}catch(_){}}
function preserveAnchorNavigation(ev){try{var link=ev.target&&ev.target.closest&&ev.target.closest("a[href]");if(!link||link.hasAttribute("download"))return;if(picking)clearCandidate();var url=new URL(link.href,w.location.href),root=w.location.pathname.match(/^(.*\/raw\/[^/]+\/)/),bundlePrefix=root&&new URL(root[1],w.location.href).href,inBundle=!!bundlePrefix&&url.href.startsWith(bundlePrefix),outbound=(url.protocol==="http:"||url.protocol==="https:")&&!inBundle;if(outbound){ev.preventDefault();post({type:"anchor:navigate",href:url.href});return;}if(link.target&&link.target!=="_self")return;if(!inBundle)return;url.searchParams.set("anchor","1");link.href=url.href;}catch(_){}}
d.addEventListener("click",preserveAnchorNavigation,true);w.addEventListener("message",receive);w.addEventListener("resize",repaint);d.addEventListener("scroll",repaint,true);w.addEventListener("load",function(){post({type:"anchor:ready",page:page});repaint();});post({type:"anchor:ready",page:page});
}catch(_){}})();</script>"###;

const PRIVATE_CACHE_CONTROL: &str = "private, max-age=60";
const PUBLIC_SHARE_CACHE_CONTROL: &str = "no-store";
const PAGE_INITIALIZER: &str = "var d=document,w=window,page=null";

/// Cache policy for the raw response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RawCachePolicy {
    /// Signed-in raw/history delivery: browser-private, one-minute cache.
    #[default]
    Private,
    /// Token-gated public share delivery: immediately revocable and not indexable.
    PublicShare,
}

/// Representation switches applied after U07 has resolved and read an [`ArtifactFile`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArtifactResponseOptions<'a> {
    /// Add the anchor bridge to HTML unless this is also a download representation.
    pub anchor: bool,
    /// Strip script blocks from HTML preview representations.
    pub preview: bool,
    /// Sanitized bundle page path supplied by U07/the raw route.
    pub page_path: Option<&'a str>,
    /// Single-artifact title used to derive the attachment filename.
    pub download_title: Option<&'a str>,
    /// Cache/indexing behavior for signed-in raw versus public-share delivery.
    pub cache: RawCachePolicy,
}

/// Match `isHtmlContentType` from `lib/artifact-http.js`.
#[must_use]
pub fn is_html_content_type(content_type: &str) -> bool {
    const PREFIX: &str = "text/html";
    content_type
        .get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        && matches!(content_type.as_bytes().get(PREFIX.len()), None | Some(b';'))
}

/// Port the single-file download naming expression from `lib/app.js:476-478`.
#[must_use]
pub fn download_name(title: &str) -> String {
    let source = if title.is_empty() { "artifact" } else { title };
    let mut cleaned = String::with_capacity(source.len().min(80));
    let mut pending_dash = false;
    for character in source.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
            if pending_dash && !cleaned.is_empty() {
                cleaned.push('-');
            }
            pending_dash = false;
            cleaned.push(character);
        } else {
            pending_dash = true;
        }
    }
    let cleaned = cleaned.trim_matches('-');
    let base = cleaned.get(..cleaned.len().min(80)).unwrap_or(cleaned);
    let base = if base.is_empty() { "artifact" } else { base };
    format!("{base}.html")
}

/// Inject the server-owned bridge before the last matching `</body>` tag, or append it.
#[must_use]
pub fn inject_anchor_bridge(content: &[u8], page_path: Option<&str>) -> Vec<u8> {
    let html = String::from_utf8_lossy(content);
    let bridge = page_path.map_or_else(
        || ANCHOR_BRIDGE.to_owned(),
        |path| {
            let encoded = serde_json::to_string(path)
                .unwrap_or_else(|_| "null".to_owned())
                .replace('<', "\\u003c");
            let replacement = format!("var d=document,w=window,page={encoded}");
            js_replace_first(ANCHOR_BRIDGE, PAGE_INITIALIZER, &replacement)
        },
    );
    let insertion = last_closing_tag(&html, "body").unwrap_or(html.len());
    let mut output = String::with_capacity(html.len() + bridge.len());
    output.push_str(&html[..insertion]);
    output.push_str(&bridge);
    output.push_str(&html[insertion..]);
    output.into_bytes()
}

/// Remove the same closed `<script>...</script>` blocks as the Node preview representation.
///
/// This is console-noise suppression, not a security boundary. Preview iframes remain sandboxed.
#[must_use]
pub fn strip_scripts(content: &[u8]) -> Vec<u8> {
    let html = String::from_utf8_lossy(content);
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some((start, end)) = next_script_block(&html, cursor) {
        output.push_str(&html[cursor..start]);
        cursor = end;
    }
    output.push_str(&html[cursor..]);
    output.into_bytes()
}

/// Exact header map returned by `rawArtifactHeaders` before listener middleware adds
/// `no-transform`.
///
/// # Errors
/// Returns [`AppError::Internal`] only if a caller bypasses U07 and supplies an invalid header
/// value as its content type.
pub fn raw_artifact_headers(
    content_type: &str,
    attachment_name: Option<&str>,
) -> Result<HeaderMap, AppError> {
    let mut headers = HeaderMap::new();
    let content_type = HeaderValue::from_str(content_type).map_err(|_| AppError::Internal)?;
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PRIVATE_CACHE_CONTROL),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DOCUMENT_SANDBOX),
    );
    if let Some(name) = attachment_name {
        let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .map_err(|_| AppError::Internal)?;
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    Ok(headers)
}

/// Build a raw response from U07's already-resolved bytes and MIME type.
///
/// # Errors
/// Returns [`AppError::Internal`] only if the supplied [`ArtifactFile::content_type`] is not a
/// legal HTTP header value. U07 emits only its frozen static MIME table.
pub fn artifact_response(
    file: ArtifactFile,
    options: ArtifactResponseOptions<'_>,
) -> Result<Response<Body>, AppError> {
    let html = is_html_content_type(&file.content_type);
    let attachment_name = options.download_title.map(download_name);
    let mut content = file.content;
    if options.anchor && attachment_name.is_none() && html {
        content = inject_anchor_bridge(&content, options.page_path);
    }
    if options.preview && html {
        content = strip_scripts(&content);
    }

    let mut headers = raw_artifact_headers(&file.content_type, attachment_name.as_deref())?;
    if options.cache == RawCachePolicy::PublicShare {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(PUBLIC_SHARE_CACHE_CONTROL),
        );
        headers.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    }
    let mut response = Response::new(Body::from(content));
    *response.headers_mut() = headers;
    Ok(response)
}

fn last_closing_tag(html: &str, tag: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let needle = format!("</{tag}");
    let mut from = 0;
    let mut last = None;
    while let Some(start) = find_ascii_case_insensitive(bytes, needle.as_bytes(), from) {
        let mut cursor = start + needle.len();
        cursor = skip_js_whitespace(html, cursor);
        if bytes.get(cursor) == Some(&b'>') {
            last = Some(start);
        }
        from = start + 1;
    }
    last
}

fn next_script_block(html: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = html.as_bytes();
    let mut search = from;
    while let Some(start) = find_ascii_case_insensitive(bytes, b"<script", search) {
        let after_name = start + b"<script".len();
        let boundary = html[after_name..]
            .chars()
            .next()
            .is_none_or(|character| !(character.is_ascii_alphanumeric() || character == '_'));
        if !boundary {
            search = start + 1;
            continue;
        }
        let open_end = bytes[after_name..]
            .iter()
            .position(|byte| *byte == b'>')
            .map(|offset| after_name + offset + 1)?;
        let mut close_search = open_end;
        while let Some(close) = find_ascii_case_insensitive(bytes, b"</script", close_search) {
            let cursor = skip_js_whitespace(html, close + b"</script".len());
            if bytes.get(cursor) == Some(&b'>') {
                return Some((start, cursor + 1));
            }
            close_search = close + 1;
        }
        return None;
    }
    None
}

fn skip_js_whitespace(value: &str, mut cursor: usize) -> usize {
    while let Some(character) = value[cursor..].chars().next() {
        if !is_js_whitespace(character) {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|offset| from + offset)
}

fn js_replace_first(haystack: &str, needle: &str, replacement: &str) -> String {
    let Some(start) = haystack.find(needle) else {
        return haystack.to_owned();
    };
    let end = start + needle.len();
    let prefix = &haystack[..start];
    let suffix = &haystack[end..];
    let mut expanded = String::with_capacity(replacement.len());
    let mut characters = replacement.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '$' {
            expanded.push(character);
            continue;
        }
        match characters.peek().copied() {
            Some('$') => {
                characters.next();
                expanded.push('$');
            }
            Some('&') => {
                characters.next();
                expanded.push_str(needle);
            }
            Some('`') => {
                characters.next();
                expanded.push_str(prefix);
            }
            Some('\'') => {
                characters.next();
                expanded.push_str(suffix);
            }
            _ => expanded.push('$'),
        }
    }
    format!("{prefix}{expanded}{suffix}")
}
