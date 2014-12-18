extern crate syntax;
use syntax::ast;
use syntax::parse::{new_parse_sess};
use syntax::parse::{ParseSess};
use syntax::parse::{new_parser_from_source_str};
use syntax::parse::parser::Parser;
use syntax::parse::token;
use syntax::visit;
use syntax::codemap;
use std::task;
use super::Match;
use super::nameres::{resolve_path_with_str};
use super::typeinf;
use super::{Scope,Ty,MatchType};
use super::Ty::{TyTuple, TyPathSearch, TyMatch, TyUnsupported};
use syntax::ptr::P;
use syntax::visit::Visitor;
use super::nameres;

// This code ripped from libsyntax::util::parser_testing
pub fn string_to_parser<'a>(ps: &'a ParseSess, source_str: String) -> Parser<'a> {
    new_parser_from_source_str(ps,
                               Vec::new(),
                               "bogofile".to_string(),
                               source_str)
}

fn with_error_checking_parse<T>(s: String, f: |&mut Parser| -> T) -> T {
    let ps = new_parse_sess();
    let mut p = string_to_parser(&ps, s);
    let x = f(&mut p);
    p.abort_if_errors();
    x
}

// parse a string, return an expr
pub fn string_to_expr (source_str : String) -> P<ast::Expr> {
    with_error_checking_parse(source_str, |p| {
        p.parse_expr()
    })
}

// parse a string, return an item
pub fn string_to_item (source_str : String) -> Option<P<ast::Item>> {
    with_error_checking_parse(source_str, |p| {
        p.parse_item(Vec::new())
    })
}

// parse a string, return a stmt
pub fn string_to_stmt(source_str : String) -> P<ast::Stmt> {
    with_error_checking_parse(source_str, |p| {
        p.parse_stmt(Vec::new())
    })
}

// parse a string, return a crate.
pub fn string_to_crate (source_str : String) -> ast::Crate {
    with_error_checking_parse(source_str, |p| {
        p.parse_crate_mod()
    })
}

#[deriving(Show)]
pub struct ViewItemVisitor {
    pub ident : Option<String>,
    pub paths : Vec<super::Path>,
    pub is_glob: bool
}

impl<'v> visit::Visitor<'v> for ViewItemVisitor {
    fn visit_view_item(&mut self, i: &ast::ViewItem) {
        match i.node {
            ast::ViewItemUse(ref path) => {
                match path.node {
                    ast::ViewPathSimple(ident, ref path, _) => {
                        self.paths.push(to_racer_path(path));
                        self.ident = Some(token::get_ident(ident).get().to_string());
                    },
                    ast::ViewPathList(ref pth, ref paths, _) => {
                        let basepath = to_racer_path(pth);
                        for path in paths.iter() {
                            match path.node {
                                ast::PathListIdent{name, ..} => {
                                    let name = token::get_ident(name).get().to_string();
                                    let seg = super::PathSegment{ name: name, types: Vec::new() };
                                    let mut newpath = basepath.clone();

                                    newpath.segments.push(seg);
                                    self.paths.push(newpath);
                                }
                                ast::PathListMod{..} => (), // TODO
                            }
                        }
                    }
                    ast::ViewPathGlob(ref pth, _) => {
                        let basepath = to_racer_path(pth);
                        self.paths.push(basepath);
                        self.is_glob = true;
                    }
                }
            },
            ast::ViewItemExternCrate(ident, ref loc, _) => {
                self.ident = Some(token::get_ident(ident).get().to_string());

                let ll = loc.clone();
                ll.map(|(ref istr, _ /* str style */)| {
                    let name = istr.get().to_string();

                    let pathseg = super::PathSegment{ name: name, 
                                                      types: Vec::new() };

                    let path = super::Path{ global: true, 
                                 segments: vec!(pathseg) };
                    self.paths.push(path);
                });
            }
        }

        visit::walk_view_item(self, i)
    }
}


pub struct LetVisitor {
    ident_points: Vec<(uint,uint)>
}

impl<'v> visit::Visitor<'v> for LetVisitor {

    fn visit_local(&mut self, local: &'v ast::Local) {  
        // don't visit the RHS (init) side of the let stmt
        self.visit_pat(&*local.pat);
    }

    fn visit_expr(&mut self, ex: &'v ast::Expr) { 
        // don't visit the RHS or block of an 'if let' stmt
        if let ast::ExprIfLet(ref pattern, _,_,_) = ex.node {
            self.visit_pat(&**pattern);
        } else {
            visit::walk_expr(self, ex) 
        }
    }

    fn visit_pat(&mut self, p: &'v ast::Pat) {
        match p.node {
            ast::PatIdent(_ , ref spannedident, _) => {
                let codemap::BytePos(lo) = spannedident.span.lo;
                let codemap::BytePos(hi) = spannedident.span.hi;
                self.ident_points.push((lo as uint, hi as uint));
            }
            _ => {
                visit::walk_pat(self, p);
            }
        }
    }
}

pub struct PatVisitor {
    ident_points: Vec<(uint,uint)>
}

impl<'v> visit::Visitor<'v> for PatVisitor {
    fn visit_pat(&mut self, p: &'v ast::Pat) {
        match p.node {
            ast::PatIdent(_ , ref spannedident, _) => {
                let codemap::BytePos(lo) = spannedident.span.lo;
                let codemap::BytePos(hi) = spannedident.span.hi;
                self.ident_points.push((lo as uint, hi as uint));
            }
            _ => {
                visit::walk_pat(self, p);
            }
        }
    }
}


fn to_racer_ty(ty: &ast::Ty, scope: &Scope) -> Option<Ty> {
    return match ty.node {
        ast::TyTup(ref items) => {
            let mut res = Vec::new();
            for t in items.iter() {
                res.push(match to_racer_ty(&**t, scope) {
                    Some(t) => t,
                    None => return None
                });
            }
            Some(TyTuple(res))
        },
        ast::TyRptr(_, ref ty) => {
            to_racer_ty(&*ty.ty, scope)
        },
        ast::TyPath(ref path, _) => {
            Some(TyPathSearch(to_racer_path(path), scope.clone()))
        }
        _ => None
    }
}


fn point_is_in_span(point: u32, span: &codemap::Span) -> bool {
    let codemap::BytePos(lo) = span.lo;
    let codemap::BytePos(hi) = span.hi;
    return point >= lo && point < hi;
}

// The point must point to an ident within the pattern.
fn destructure_pattern_to_ty(pat: &ast::Pat, 
                             point: uint, 
                             ty: &Ty, 
                             scope: &Scope) -> Option<Ty> {
    debug!("destructure_pattern_to_ty point {} ty {}    ||||||||    pat: {}",point, ty, pat);
    return match pat.node {
        ast::PatIdent(_ , ref spannedident, _) => {
            if point_is_in_span(point as u32, &spannedident.span) {
                debug!("destructure_pattern_to_ty matched an ident!");
                return Some(ty.clone());
            } else {
                panic!("Expecting the point to be in the patident span. pt: {}", point);
            }
        }
        ast::PatTup(ref tuple_elements) => {
            return match ty {
                &TyTuple(ref typeelems) => {
                    let mut i = 0u;
                    let mut res = None;
                    for p in tuple_elements.iter() {
                        if point_is_in_span(point as u32, &p.span) {
                            let ref ty = typeelems[i];
                            res = destructure_pattern_to_ty(&**p, point, ty, scope);
                            break;
                        }
                        i += 1;
                    }
                    res
                }
                _ => panic!("Expecting TyTuple")
                
            }
        }
        ast::PatEnum(ref path, ref children) => {
            let mut i = 0u;

            let m = resolve_ast_path(path, &scope.filepath, scope.point);

            debug!("PHIL patenum path resolved to {}",m);

            let contextty = path_to_match(ty.clone());
            if let (Some(m), Some(children)) = (m, children.as_ref()) {
                let mut res = None;
                for p in children.iter() {
                    if point_is_in_span(point as u32, &p.span) {
                        
                        res = typeinf::get_tuplestruct_field_type(i, &m)
                            .and_then(|ty|
                                // if context ty is a match, use its generics
                                if let Some(Ty::TyMatch(ref contextmatch)) = contextty {
                                    path_to_match_including_generics(ty, contextmatch)
                                } else {
                                    path_to_match(ty)
                                })
                            .and_then(|ty| destructure_pattern_to_ty(&**p, point, &ty, scope));

                        break;
                    }
                    i += 1;
                }
                res
            } else {
                None
            }
        }
        _ => {
            debug!("Could not destructure pattern {}", pat);
            None
        }
    }
}

struct LetTypeVisitor {
    scope: Scope,
    srctxt: String,
    pos: uint,        // pos is relative to the srctxt, scope is global
    result: Option<Ty>
}

impl<'v> visit::Visitor<'v> for LetTypeVisitor {
    
    fn visit_expr(&mut self, ex: &'v ast::Expr) { 
        if let ast::ExprIfLet(ref pattern, ref expr, _, _) = ex.node {
            debug!("PHIL lettypevisitor - iflet pattern");
            let mut v = ExprTypeVisitor{ scope: self.scope.clone(),
                                         result: None};
            v.visit_expr(&**expr);

            debug!("PHIL lettypevisitor: expr is {}", v.result);

            self.result = v.result.and_then(|ty|
                   destructure_pattern_to_ty(&**pattern, self.pos, 
                                             &ty, &self.scope))
                .and_then(|ty| path_to_match(ty));
        } else {
            visit::walk_expr(self, ex) 
        }
    }



    fn visit_local(&mut self, local: &'v ast::Local) {
        let mut ty = to_racer_ty(&*local.ty, &self.scope);
        
        if ty.is_none() {
            // oh, no type in the let expr. Try evalling the RHS
            ty = local.init.as_ref().and_then(|initexpr| {
                debug!("init node is {}",initexpr.node);
                let mut v = ExprTypeVisitor{ scope: self.scope.clone(),
                                             result: None};
                v.visit_expr(&**initexpr);
                v.result
            });
        }

        debug!("LetTypeVisitor: ty is {}. pos is {}, src is |{}|",ty, self.pos, self.srctxt);
        self.result = ty.and_then(|ty|
           destructure_pattern_to_ty(&*local.pat, self.pos, &ty, &self.scope))
            .and_then(|ty| path_to_match(ty));
    }
}

fn resolve_ast_path(path: &ast::Path, filepath: &Path, pos: uint) -> Option<Match> {
    debug!("resolve_ast_path {}",to_racer_path(path));
    return nameres::resolve_path_with_str(&to_racer_path(path), filepath, pos, super::SearchType::ExactMatch, super::Namespace::BothNamespaces).nth(0);
}

fn to_racer_path(pth: &ast::Path) -> super::Path {

    let mut v = Vec::new();    
    for seg in pth.segments.iter() {
        let name = token::get_ident(seg.identifier).get().to_string();
        let mut types = Vec::new();    
        for ty in seg.parameters.types().iter() {
            match ty.node {
                ast::TyPath(ref path, _) => {
                    types.push(to_racer_path(path));
                }
                _ => {}
            }
        }
        v.push(super::PathSegment{ name: name, types: types}); 
    }
    return super::Path{ global: pth.global, segments: v} ;
}

fn path_to_match(ty: Ty) -> Option<Ty> {
    return match ty {
        TyPathSearch(ref path, ref scope) => 
            find_type_match(path, &scope.filepath, scope.point),
        _ => Some(ty)
    };
}

fn find_type_match(path: &super::Path, fpath: &Path, pos: uint) -> Option<Ty> {
    debug!("find_type_match {}",path);
    let res = resolve_path_with_str(path, fpath, pos, super::SearchType::ExactMatch,
               super::Namespace::TypeNamespace).nth(0).and_then(|m| {
                   match m.mtype {
                       super::MatchType::Type => get_type_of_typedef(m),
                       _ => Some(m)
                   }
               });

    return res.and_then(|m|{
        // add generic types to match (if any)
        let types: Vec<super::PathSearch> = path.generic_types()
            .map(|typepath| 
                 super::PathSearch{ 
                     path: typepath.clone(),
                     filepath: fpath.clone(),
                     point: pos
                 }).collect();

        if types.is_empty() {
            return Some(TyMatch(m));
        } else {
            return Some(TyMatch(m.with_generic_types(types.clone())));
        }
    });
}

fn get_type_of_typedef(m: Match) -> Option<Match> {
    debug!("get_type_of_typedef match is {}",m);
    let msrc = super::load_file_and_mask_comments(&m.filepath);
    let blobstart = m.point - 5;  // - 5 because 'type '
    let blob = msrc.as_slice().slice_from(blobstart);

    return super::codeiter::iter_stmts(blob).nth(0).and_then(|(start, end)|{
        let blob = msrc.as_slice().slice(blobstart + start, blobstart+end).to_string();
        debug!("get_type_of_typedef blob string {}",blob);
        let res = parse_type(blob);
        debug!("get_type_of_typedef parsed type {}",res.type_);
        return res.type_;
    }).and_then(|type_|{
        nameres::resolve_path_with_str(&type_, &m.filepath, m.point, super::SearchType::ExactMatch, super::Namespace::TypeNamespace).nth(0)
    });
}


struct ExprTypeVisitor {
    scope: Scope,
    result: Option<Ty>
}

impl<'v> visit::Visitor<'v> for ExprTypeVisitor {
    fn visit_expr(&mut self, expr: &ast::Expr) {
        debug!("visit_expr {}",expr);
        //walk_expr(self, ex, e) 
        match expr.node {
            ast::ExprPath(ref path) => {
                debug!("expr is a path {}",to_racer_path(path));
                self.result = resolve_ast_path(path, 
                                 &self.scope.filepath, 
                                 self.scope.point).and_then(|m| {
                   let msrc = super::load_file_and_mask_comments(&m.filepath);
                   typeinf::get_type_of_match(m, msrc.as_slice())
                                 });
            }
            ast::ExprCall(ref callee_expression, _/*ref arguments*/) => {
                self.visit_expr(&**callee_expression);

                self.result = self.result.as_ref().and_then(|m|
                    match m {
                        &TyMatch(ref m) =>  {

                            match m.mtype {
                                MatchType::Function => typeinf::get_return_type_of_function(m)
                                    .and_then(path_to_match),
                                MatchType::Struct => Some(TyMatch(m.clone())),
                                _ => {
                                    debug!("ExprTypeVisitor: Cannot handle ExprCall of {} type", m.mtype);
                                    None
                                }
                            }
                        },
                        _ => None
                    }
                );

            }
            ast::ExprStruct(ref path, _, _) => {
                let pathvec = to_racer_path(path);
                self.result = find_type_match(&pathvec,
                                              &self.scope.filepath,
                                              self.scope.point);
            }

            ast::ExprMethodCall(ref spannedident, ref types, ref arguments) => {
                // spannedident.node is an ident I think
                let methodname = token::get_ident(spannedident.node).get().to_string();
                debug!("method call ast name {}",methodname);
                debug!("method call ast types {} {}",types, types.len());
                
                let objexpr = &arguments[0];
                //println!("obj expr is {:?}",objexpr);
                self.visit_expr(&**objexpr);

                self.result = self.result.as_ref().and_then(|contextm|{
                    match contextm {
                        &TyMatch(ref contextm) => {
                            let omethod = nameres::search_for_impl_methods(
                                contextm.matchstr.as_slice(),
                                methodname.as_slice(), 
                                contextm.point, 
                                &contextm.filepath,
                                contextm.local,
                                super::SearchType::ExactMatch).nth(0);
                            omethod
                                .and_then(|method| 
                                          super::typeinf::get_return_type_of_function(&method))
                                .and_then(|ty| path_to_match_including_generics(ty, contextm))
                        }
                        _ => None
                    }
                });
            }

            ast::ExprField(ref subexpression, spannedident) => {
                let fieldname = token::get_ident(spannedident.node).get().to_string();
                debug!("exprfield {}",fieldname);
                self.visit_expr(&**subexpression);
                self.result = self.result.as_ref()
                      .and_then(|structm| 
                                match structm {
                                    &TyMatch(ref structm) => {
                                typeinf::get_struct_field_type(fieldname.as_slice(), structm)
                                .and_then(|fieldtypepath| 
                                          find_type_match_including_generics(&fieldtypepath,
                                                                             &structm.filepath,
                                                                             structm.point,
                                                                             structm))
                                    },
                                    _ => None
                                });

            }

            ast::ExprTup(ref exprs) => {
                let mut v = Vec::new();
                for expr in exprs.iter() {
                    self.visit_expr(&**expr);
                    match self.result {
                        Some(ref t) => v.push(t.clone()), 
                        None => {
                            self.result = None;
                            return;
                        }
                    };
                }
                self.result = Some(TyTuple(v));
            }

            ast::ExprLit(_) => {
                self.result = Some(TyUnsupported);
            }

            _ => {
                println!("- Could not match expr node type: {}",expr.node);
            }
        }
    }

}

// gets generics info from the context match
fn path_to_match_including_generics(ty: Ty, contextm: &super::Match) -> Option<Ty> {
    return match ty {
        TyPathSearch(ref fieldtypepath, ref scope) => {

            if fieldtypepath.segments.len() == 1 {
                // could be a generic arg! - try and resolve it
                let ref typename = fieldtypepath.segments[0].name;
                let mut it = contextm.generic_args.iter()
                    .zip(contextm.generic_types.iter());
                for (name, typesearch) in it {
                    if name == typename {
                        // yes! a generic type match!
                        return find_type_match(&typesearch.path, 
                                               &typesearch.filepath, 
                                               typesearch.point);
                    }
                }
            }

            find_type_match(fieldtypepath, &scope.filepath, scope.point)
        }
        _ => Some(ty)
    };
}


fn find_type_match_including_generics(fieldtype: &super::Ty,
                                      filepath: &Path,
                                      pos: uint,
                                      structm: &super::Match) -> Option<Ty>{

    let fieldtypepath = match fieldtype {
        &TyPathSearch(ref path, _) => path,
        _ => {
            debug!("EXPECTING A PATH!! Cannot handle other types yet. {}", fieldtype);
            return None
        }
    };


    if fieldtypepath.segments.len() == 1 {
        // could be a generic arg! - try and resolve it
        let ref typename = fieldtypepath.segments[0].name;
        let mut it = structm.generic_args.iter()
            .zip(structm.generic_types.iter());
        for (name, typesearch) in it {
            if name == typename {
                // yes! a generic type match!
                return find_type_match(&typesearch.path, 
                                       &typesearch.filepath, 
                                       typesearch.point);
            }
        }
    }
    
    return find_type_match(fieldtypepath, 
                           filepath, 
                           pos);
}


struct StructVisitor {
    pub scope: Scope,
    pub fields: Vec<(String, uint, Option<super::Ty>)>
}

impl<'v> visit::Visitor<'v> for StructVisitor {
    fn visit_struct_def(&mut self, struct_definition: &ast::StructDef, _: ast::Ident, _: &ast::Generics, _: ast::NodeId) {

        for field in struct_definition.fields.iter() {
            let codemap::BytePos(point) = field.span.lo;

            match field.node.kind {
                ast::NamedField(name, _) => {
                    let name = String::from_str(token::get_ident(name).get());
                    let ty = to_racer_ty(&*field.node.ty, &self.scope);
                    self.fields.push((name, point as uint, ty));
                }
                ast::UnnamedField(_) => {
                    let ty = to_racer_ty(&*field.node.ty, &self.scope);
                    self.fields.push(("".to_string(), point as uint, ty));
                }
            }
        }
    }
}

pub struct TypeVisitor {
    pub name: Option<String>,
    pub type_: Option<super::Path>
}

impl<'v> visit::Visitor<'v> for TypeVisitor {
    fn visit_item(&mut self, item: &ast::Item) {
        match item.node {
            ast::ItemTy(ref ty, _) => {
                self.name = Some(token::get_ident(item.ident).get().to_string());

                let typepath = match ty.node {
                    ast::TyRptr(_, ref ty) => {
                        match ty.ty.node {
                            ast::TyPath(ref path, _) => {
                                let type_ = to_racer_path(path);
                                debug!("type type is {}", type_);
                                Some(type_)
                            }
                            _ => None
                        }
                    }
                    ast::TyPath(ref path, _) => {
                        let type_ = to_racer_path(path);
                        debug!("type type is {}", type_);
                        Some(type_)
                    }
                    _ => None
                };
                self.type_ = typepath;
                debug!("typevisitor type is {}",self.type_);
            }
            _ => ()
        }
    }
}

pub struct TraitVisitor {
    pub name: Option<String>
}

impl<'v> visit::Visitor<'v> for TraitVisitor {
    fn visit_item(&mut self, item: &ast::Item) {
        match item.node {
            ast::ItemTrait(_, _, _, _, _) => {
                self.name = Some(token::get_ident(item.ident).get().to_string());
            }
            _ => ()
        }
    }
}

#[deriving(Show)]
pub struct ImplVisitor {
    pub name_path: Option<super::Path>,
    pub trait_path: Option<super::Path>,
}

impl<'v> visit::Visitor<'v> for ImplVisitor {
    fn visit_item(&mut self, item: &ast::Item) {
        match item.node {
            ast::ItemImpl(_, _, ref otrait, ref typ, _) => {
                match typ.node {
                    ast::TyPath(ref path, _) => {
                        self.name_path = Some(to_racer_path(path));
                    }
                    ast::TyRptr(_, ref ty) => {
                        // HACK for now, treat refs the same as unboxed types 
                        // so that we can match '&str' to 'str'
                        match ty.ty.node {
                            ast::TyPath(ref path, _) => {
                                self.name_path = Some(to_racer_path(path));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                otrait.as_ref().map(|ref t|{
                    self.trait_path = Some(to_racer_path(&t.path));
                });

            },
            _ => {}
        }
    }
}

// pub struct FnTypeVisitor {
//     m: Option<Match>,
//     ctx: Match
// }
//
// impl visit::Visitor<String> for FnTypeVisitor {
//     fn visit_fn(&mut self, _: &visit::FnKind, fd: &ast::FnDecl, _: &ast::Block, _: codemap::Span, _: ast::NodeId, name: String) {
//         if name.as_slice() == "return_value" {
//             match fd.output.node {
//                 ast::TyPath(ref path, _) => {
//                     //self.output = path_to_vec(path);
//                     self.m = resolve_ast_path(path,
//                                               &self.ctx.filepath,
//                                               self.ctx.point);
//                     debug!("visit_fn: return type is {}",self.m);
//                 }
//                 _ => {}
//             }
//         }
//     }
// }

#[deriving(Show)]
pub struct FnVisitor {
    pub name: String,
    pub output: Option<super::Ty>,
    pub args: Vec<(String, uint, Option<super::Path>)>,
    pub is_method: bool,
    pub scope: Scope
}

impl<'v> visit::Visitor<'v> for FnVisitor {
    fn visit_fn(&mut self, fk: visit::FnKind, fd: &ast::FnDecl, _: &ast::Block, _: codemap::Span, _: ast::NodeId) {

        let name = match fk {
            visit::FkItemFn(name, _, _, _) | visit::FkMethod(name, _, _) => name,
            visit::FkFnBlock(..) => syntax::parse::token::special_idents::invalid
        };
        self.name = token::get_ident(name).get().to_string();

        for arg in fd.inputs.iter() {
            debug!("fn arg ast is {}",arg);
            let res  = 
                match arg.pat.node {
                    ast::PatIdent(_ , ref spannedident, _) => {
                        let codemap::BytePos(point) = spannedident.span.lo;
                        let argname = token::get_ident(spannedident.node).get().to_string();
                        Some((String::from_str(argname.as_slice()), point as uint))
                    }
                    _ => None
                };

            if res.is_none() {
                return;
            }

            let (name, pos) = res.unwrap();

            let t = to_racer_ty(&*arg.ty, &self.scope);
            debug!("visit_fn arg racer_ty {}",t);

            let typepath = match arg.ty.node {
                ast::TyRptr(_, ref ty) => {
                    match ty.ty.node {
                        ast::TyPath(ref path, _) => {
                            let type_ = to_racer_path(path);
                            debug!("arg type is {}", type_);
                            Some(type_)
                        }
                        _ => None
                    }
                }
                ast::TyPath(ref path, _) => {
                    let type_ = to_racer_path(path);
                    debug!("arg type is {}", type_);
                    Some(type_)
                }
                _ => None
            };

            debug!("typepath {}", typepath);
            self.args.push((name, pos, typepath))
        }

        debug!("parsed args: {}", self.args);

        self.output = match fd.output {
            ast::Return(ref ty) =>
                to_racer_ty(&**ty, &self.scope),
            ast::NoReturn(_) =>
                None,
        };

        // match fd.output.node {
        //     ast::TyPath(ref path, _) => {
        //         self.output = Some(to_racer_path(path));
        //     }
        //     _ => {}
        // }

        self.is_method = match fk {
            visit::FkMethod(_, _, _) => true,
            _ => false
        }
    }

}

pub struct ModVisitor {
    pub name: Option<String>
}

impl<'v> visit::Visitor<'v> for ModVisitor {
    fn visit_item(&mut self, item: &ast::Item) {
        match item.node {
            ast::ItemMod(_) => {
                self.name = Some(String::from_str(token::get_ident(item.ident).get()));
            }
            _ => {}
        }
    }

}

pub struct GenericsVisitor {
    pub generic_args: Vec<String>
}

impl<'v> visit::Visitor<'v> for GenericsVisitor {
    fn visit_generics(&mut self, g: &ast::Generics) {
        for ty in g.ty_params.iter() {
            self.generic_args.push(String::from_str(token::get_ident(ty.ident).get()));
        }
    }
}

pub struct StructDefVisitor {
    pub name: Option<(String,uint)>,
    pub generic_args: Vec<String>
}

impl<'v> visit::Visitor<'v> for StructDefVisitor {
    fn visit_generics(&mut self, g: &ast::Generics) {
        for ty in g.ty_params.iter() {
            self.generic_args.push(String::from_str(token::get_ident(ty.ident).get()));
        }
    }

    fn visit_ident(&mut self, _sp: codemap::Span, _ident: ast::Ident) {
        /*! Visit the idents */
        let codemap::BytePos(point) = _sp.lo;
        let name = String::from_str(token::get_ident(_ident).get());
        self.name = Some((name,point as uint));
    }
}

pub struct EnumVisitor {
    pub name: String,
    pub values: Vec<(String, uint)>,
}

impl<'v> visit::Visitor<'v> for EnumVisitor {
    fn visit_item(&mut self, i: &ast::Item) {
        match i.node {
            ast::ItemEnum(ref enum_definition, _) => {
                self.name = String::from_str(token::get_ident(i.ident).get());
                //visitor.visit_generics(type_parameters, env.clone());
                //visit::walk_enum_def(self, enum_definition, type_parameters, e)

                let codemap::BytePos(point) = i.span.lo;
                let codemap::BytePos(point2) = i.span.hi;
                debug!("name point is {} {}",point,point2);

                for variant in enum_definition.variants.iter() {
                    let codemap::BytePos(point) = variant.span.lo;
                    self.values.push((String::from_str(token::get_ident(variant.node.name).get()), point as uint));
                }
            },
            _ => {}
        }
    }
}

pub fn parse_view_item(s: String) -> ViewItemVisitor {
    // parser can panic!() so isolate it in another task
    let result = task::try(move || { 
        let cr = string_to_crate(s);
        let mut v = ViewItemVisitor{ident: None, 
                                    paths: Vec::new(),
                                    is_glob: false};
        visit::walk_crate(&mut v, &cr);
        return v;
    });
    match result {
        Ok(s) => {return s;},
        Err(_) => {
            return ViewItemVisitor{ident: None, 
                                   paths: Vec::new(),
                                   is_glob: false};
        }
    }
}

pub fn parse_let(s: String) -> Vec<(uint, uint)> {
    return task::try(move || { 
        let stmt = string_to_stmt(s);
        let mut v = LetVisitor{ ident_points: Vec::new() };
        visit::walk_stmt(&mut v, &*stmt);
        return v.ident_points;
    }).unwrap_or(Vec::new());
}

pub fn parse_struct_fields(s: String, scope: Scope) -> Vec<(String, uint, Option<super::Ty>)> {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = StructVisitor{ scope: scope, fields: Vec::new() };
        visit::walk_stmt(&mut v, &*stmt);
        return v.fields;
    }).ok().unwrap_or(Vec::new());
}

pub fn parse_impl(s: String) -> ImplVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = ImplVisitor { name_path: None, trait_path: None };
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();    
}

pub fn parse_trait(s: String) -> TraitVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = TraitVisitor { name: None };
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();    
}

pub fn parse_struct_def(s: String) -> StructDefVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = StructDefVisitor { name: None, generic_args: Vec::new() };
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();
}

pub fn parse_generics(s: String) -> GenericsVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = GenericsVisitor { generic_args: Vec::new() };
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();
}

pub fn parse_type(s: String) -> TypeVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = TypeVisitor { name: None, type_: None };
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();    
}

pub fn parse_fn_args(s: String) -> Vec<(uint, uint)> {
    return task::try(move || { 
        let stmt = string_to_stmt(s);
        debug!("parse_fn_args stmt is {}",stmt);
        let mut v = PatVisitor{ ident_points: Vec::new() };
        visit::walk_stmt(&mut v, &*stmt);
        debug!("ident points are {}", v.ident_points);
        return v.ident_points;
    }).unwrap_or(Vec::new());
}

pub fn parse_fn_output(s: String, scope: Scope) -> Option<super::Ty> {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = FnVisitor { name: "".to_string(), args: Vec::new(), 
                                output: None, is_method: false, scope: scope};
        visit::walk_stmt(&mut v, &*stmt);
        return v.output;
    }).ok().unwrap();
}


pub fn parse_fn(s: String, scope: Scope) -> FnVisitor {
    debug!("parse_fn |{}|",s);
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = FnVisitor { name: "".to_string(), args: Vec::new(), 
                                output: None, is_method: false, scope: scope};
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();
}

pub fn parse_mod(s: String) -> ModVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = ModVisitor { name: None };
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();    
}

pub fn parse_enum(s: String) -> EnumVisitor {
    return task::try(move || {
        let stmt = string_to_stmt(s);
        let mut v = EnumVisitor { name: String::new(), values: Vec::new()};
        visit::walk_stmt(&mut v, &*stmt);
        return v;
    }).ok().unwrap();
}


pub fn get_type_of(exprstr: String, fpath: &Path, pos: uint) -> Option<Ty> {
    let myfpath = fpath.clone();

    return task::try(move || {
        let stmt = string_to_stmt(exprstr);
        let startscope = Scope {
            filepath: myfpath,
            point: pos
        };

        let mut v = ExprTypeVisitor{ scope: startscope,
                                     result: None};
        visit::walk_stmt(&mut v, &*stmt);
        return v.result;
    }).ok().unwrap();
}

// pos points to an ident in the lhs of the stmtstr
pub fn get_let_type(stmtstr: String, pos: uint, scope: Scope) -> Option<Ty> {
    return task::try(move || {
        let stmt = string_to_stmt(stmtstr.clone());
        let mut v = LetTypeVisitor {
            scope: scope,
            srctxt: stmtstr,
            pos: pos, result: None
        };
        visit::walk_stmt(&mut v, &*stmt);
        return v.result;
    }).ok().unwrap();
}


pub struct FnArgTypeVisitor {
    point: uint
    //result: Option<Ty>
}

impl Copy for FnArgTypeVisitor {}

impl<'v> visit::Visitor<'v> for FnArgTypeVisitor {

    fn visit_fn(&mut self, _: visit::FnKind, fd: &ast::FnDecl, _: &ast::Block, _: codemap::Span, _: ast::NodeId) {
        for arg in fd.inputs.iter() {
            let codemap::BytePos(lo) = arg.pat.span.lo;
            let codemap::BytePos(hi) = arg.pat.span.hi;
            if self.point >= (lo as uint) && self.point <= (hi as uint) {
                debug!("found type {}",arg.ty);
            }
        }
    }
}


//------------------------------------- SPIKE ----------------

// struct Expr {
//     expr: String,
//     fpath: Path,
//     point: uint
// }

//------------------------------------------------------------


#[test]
fn ast_sandbox() {
    //let src = "if let Foo(a) = b {}";

    //let src = "let (a,b): (Foo,Bar);";

    //let src = "let (a,b) = (2,3);";

    // let src = "let (a,b) : (uint,uint);";

    // //let src = "fn foo(a: int, b: |int|->int) {};";

    // let src = "let myvar = |blah| {};";

    // let src = "(myvar, foo) = (3,4);";

    // let src = "fn myfn((a,b) : (uint, uint)) {}";
    // //let src = "impl blah {pub fn another_method() {}}";

    // let stmt = string_to_stmt(String::from_str(src));
    // println!("stmt {} ", stmt);
    // panic!("");
    // let mut v = PatVisitor { ident_points: Vec::new() };
    // visit::walk_stmt(&mut v, &*stmt);

    // panic!("BLAH {}",v.result);

    // let mut v = LetTypeVisitor { scope: Scope {filepath: Path::new("./foo"), point: 0 },
    //                           srctxt: src.to_string(),
    //                           pos: 5, result: None
    // };
    // visit::walk_stmt(&mut v, &*stmt);

    // let out = parse_let2(src.to_string());

    // for &(l,h) in out.iter() {
    //     println!("{}",src.slice(l,h));
    // }

    // let src = "let (a,b) : (uint,uint);";

    // let result = task::try(move || { 
    //     return string_to_stmt(String::from_str(src));
    // });


    //println!("out {} ", result);
    //panic!();

    //parse_let("let l : Vec<Blah>;".to_string(), Path::new("./ast.rs"), 0, true);
    //parse_let("let l = Vec<Blah>::new();".to_string(), Path::new("./ast.rs"), 0, true);

    //get_type_of("let l : Vec<Blah>;".to_string(), &Path::new("./ast.rs"), 0);
    //panic!();


    // let src = "pub struct Foo<T>;";
    // let s = parse_generics(src.to_string());
    // println!("out {} ", s.generic_args);
    // let stmt = string_to_stmt(String::from_str(src));
    // println!("stmt is {:?}",stmt);
    // panic!();
    // // let mut v = LetVisitor{ scope: Scope {filepath: Path::new("./foo"), point: 0} , result: None, parseinit: true};
    // let mut v = StructDefVisitor {nam}
    // visit::walk_stmt(&mut v, &*stmt, ());

    // println!("{:?}", stmt);
    // panic!();
    // println!("{}", v.name);
    // panic!("");
    // let mut v = ExprTypeVisitor{ scope: startscope,
    //                              result: None};
    

    // let src = ~"fn myfn(a: uint) -> Foo {}";
    // let src = ~"impl blah{    fn visit_item(&mut self, item: &ast::Item, _: ()) {} }";

    // let src = "Foo::Bar().baz(32)";
    //let src = "std::vec::Vec::new().push_all()";
    // let src = "impl visit::Visitor<()> for ExprTypeVisitor {}";
    // let src = "impl<'a> StrSlice<'a> for &'a str {}";

    //let src = "a(|b|{});";
    // let src = "(a,b) = (3,4);";
    // let src = "fn foo() -> (a,b) {}";

    // let stmt = string_to_stmt(String::from_str(src));

    // let src = "extern crate core_collections = \"collections\";";
    // let src = "use foo = bar;";
    // let src = "use bar::baz;";

    // let src = "use bar::{baz,blah};";
    // let src = "extern crate collections;";
    // let cr = string_to_crate(String::from_str(src));
    // let mut v = ViewItemVisitor{ ident: None, paths: Vec::new() };
    // visit::walk_crate(&mut v, &cr, ());

    // println!("v {} {}",v.ident, v.paths);

    //visit::walk_stmt(&mut v, stmt, ());

    // println!("stmt {:?}",stmt);


    // let mut v = ImplVisitor{ name_path: Vec::new() };
    // visit::walk_stmt(&mut v, stmt, ());


    // println!("v {}",v.name_path);

// pub struct Match {
//     pub matchstr: ~str,
//     pub filepath: Path,
//     pub point: uint,
//     pub linetxt: ~str,
//     pub local: bool,
//     pub mtype: MatchType
// }

    // let startscope = Match{ 
    //     matchstr: "".to_owned(),
    //     filepath: Path::new("./ast.rs"),
    //     point: 0,
    //     linetxt: "".to_owned(),
    //     local: true,
    //     mtype: super::Module
    // };

    // let startscope = Scope {
    //     filepath: Path::new("./ast.rs"),
    //     point: 0
    // };

    // let mut v = ExprTypeVisitor{ scope: startscope,
    //                              result: None};
    // visit::walk_stmt(&mut v, stmt, ());

    // println!("result was {:?}",v.result);
    //return v.result;

    // let mut v = EnumVisitor { name: String::new(), values: Vec::new()};
    // visit::walk_stmt(&mut v, cr, ());
    // println!("{} {}", v.name, v.values);

    // let src = "let v = Foo::new();".to_owned();

    // let res = parse_let(src);
    // debug!("res {}",res.unwrap().init);
}

