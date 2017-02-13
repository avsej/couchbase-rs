//! Bucket-level operations and API.
use std::ptr;
use couchbase_sys::*;
use std::ffi::CString;
use std::sync::Arc;
use parking_lot::Mutex;
use std::thread;
use std::thread::{park, JoinHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use futures::sync::oneshot::{channel, Sender};
use futures::sync::mpsc::{unbounded, UnboundedSender};
use {CouchbaseStream, CouchbaseFuture, N1qlResult, Document, CouchbaseError, N1qlRow, ViewResult,
     ViewRow, ViewMeta};
use std;
use std::slice;
use serde_json;

type FutureSender<T> = *mut Sender<Result<T, CouchbaseError>>;
type StreamSender<T> = *mut UnboundedSender<Result<T, CouchbaseError>>;

pub struct Bucket {
    instance: Arc<Mutex<SendPtr<lcb_t>>>,
    io_handle: Mutex<Option<JoinHandle<()>>>,
    io_running: Arc<AtomicBool>,
}

impl Bucket {
    pub fn new<'a>(cs: &'a str, pw: &'a str) -> Result<Self, CouchbaseError> {
        let mut instance: lcb_t = ptr::null_mut();

        let connstr = CString::new(cs).unwrap();
        let passstr = CString::new(pw).unwrap();

        let mut cropts = lcb_create_st {
            version: 3,
            v: unsafe { ::std::mem::zeroed() },
        };

        let boot_result = unsafe {
            cropts.v.v3.as_mut().connstr = connstr.as_ptr();
            cropts.v.v3.as_mut().passwd = passstr.as_ptr();

            lcb_create(&mut instance, &cropts);

            let mut enable = true;
            let cntl_res = lcb_cntl(instance,
                                    LCB_CNTL_SET as i32,
                                    LCB_CNTL_DETAILED_ERRCODES as i32,
                                    &mut enable as *mut bool as *mut std::os::raw::c_void);
            if cntl_res != LCB_SUCCESS {
                return Err(cntl_res.into());
            }

            lcb_connect(instance);
            lcb_wait(instance);
            lcb_get_bootstrap_status(instance)
        };

        if boot_result != LCB_SUCCESS {
            return Err(boot_result.into());
        }

        // install the generic callbacks
        unsafe {
            lcb_install_callback3(instance, LCB_CALLBACK_GET as i32, Some(get_callback));
            lcb_install_callback3(instance, LCB_CALLBACK_STORE as i32, Some(store_callback));
            lcb_install_callback3(instance, LCB_CALLBACK_REMOVE as i32, Some(remove_callback));
        }

        let mt_instance = Arc::new(Mutex::new(SendPtr { inner: Some(instance) }));
        let io_running = Arc::new(AtomicBool::new(true));

        let io_instance = mt_instance.clone();
        let still_running = io_running.clone();
        let handle = thread::Builder::new()
            .name("io".into())
            .spawn(move || loop {
                park();
                if !still_running.load(Ordering::Acquire) {
                    break;
                }
                let guard = io_instance.lock();
                let instance = guard.inner.unwrap();
                unsafe { lcb_wait(instance) };
            })
            .unwrap();

        Ok(Bucket {
            instance: mt_instance,
            io_handle: Mutex::new(Some(handle)),
            io_running: io_running,
        })
    }

    fn unpark_io(&self) {
        let guard = self.io_handle.lock();
        guard.as_ref().unwrap().thread().unpark();
    }

    /// Fetch a `Document` from the `Bucket`.
    pub fn get<S>(&self, id: S) -> CouchbaseFuture<Document>
        where S: Into<String>
    {
        let (tx, rx) = channel();

        let idm = id.into();
        let idm_len = idm.len();
        let lcb_id = CString::new(idm).unwrap();
        let mut cmd: lcb_CMDGET = unsafe { ::std::mem::zeroed() };
        cmd.key.type_ = LCB_KV_COPY;
        cmd.key.contig.bytes = lcb_id.into_raw() as *const std::os::raw::c_void;
        cmd.key.contig.nbytes = idm_len as usize;

        let tx_boxed = Box::new(tx);

        unsafe {
            let guard = self.instance.lock();
            let instance = guard.inner.unwrap();
            lcb_get3(instance,
                     Box::into_raw(tx_boxed) as *const std::os::raw::c_void,
                     &cmd as *const lcb_CMDGET);
        }

        self.unpark_io();
        CouchbaseFuture::new(rx)
    }

    /// Remove a `Document` from the `Bucket`.
    pub fn remove<S>(&self, id: S) -> CouchbaseFuture<()>
        where S: Into<String>
    {
        let (tx, rx) = channel();

        let idm = id.into();
        let idm_len = idm.len();
        let lcb_id = CString::new(idm).unwrap();
        let mut cmd: lcb_CMDREMOVE = unsafe { ::std::mem::zeroed() };
        cmd.key.type_ = LCB_KV_COPY;
        cmd.key.contig.bytes = lcb_id.into_raw() as *const std::os::raw::c_void;
        cmd.key.contig.nbytes = idm_len as usize;

        let tx_boxed = Box::new(tx);

        unsafe {
            let guard = self.instance.lock();
            let instance = guard.inner.unwrap();
            lcb_remove3(instance,
                        Box::into_raw(tx_boxed) as *const std::os::raw::c_void,
                        &cmd as *const lcb_CMDREMOVE);
        }

        self.unpark_io();
        CouchbaseFuture::new(rx)
    }

    /// Insert a `Document` into the `Bucket`.
    pub fn insert(&self, document: Document) -> CouchbaseFuture<Document> {
        self.store(document, LCB_ADD)
    }

    /// Upsert a `Document` into the `Bucket`.
    pub fn upsert(&self, document: Document) -> CouchbaseFuture<Document> {
        self.store(document, LCB_UPSERT)
    }

    /// Replace a `Document` in the `Bucket`.
    pub fn replace(&self, document: Document) -> CouchbaseFuture<Document> {
        self.store(document, LCB_REPLACE)
    }

    fn store(&self, document: Document, operation: lcb_storage_t) -> CouchbaseFuture<Document> {
        let (tx, rx) = channel();

        let lcb_id = CString::new(document.id()).unwrap();
        let mut cmd: lcb_CMDSTORE = unsafe { ::std::mem::zeroed() };
        cmd.operation = operation;
        cmd.exptime = document.expiry();
        cmd.key.type_ = LCB_KV_COPY;
        cmd.key.contig.bytes = lcb_id.into_raw() as *const std::os::raw::c_void;
        cmd.key.contig.nbytes = document.id().len() as usize;

        let content = document.content();
        let content_len = content.len();
        let lcb_content = CString::new(content).unwrap();
        cmd.value.vtype = LCB_KV_COPY;
        unsafe {
            cmd.value.u_buf.contig.as_mut().bytes = lcb_content.into_raw() as
                                                    *const std::os::raw::c_void;
            cmd.value.u_buf.contig.as_mut().nbytes = content_len as usize;
        }
        let tx_boxed = Box::new(tx);

        unsafe {
            let guard = self.instance.lock();
            let instance = guard.inner.unwrap();

            lcb_store3(instance,
                       Box::into_raw(tx_boxed) as *const std::os::raw::c_void,
                       &cmd as *const lcb_CMDSTORE);
        }

        self.unpark_io();
        CouchbaseFuture::new(rx)
    }

    pub fn query_n1ql<S>(&self, query: S) -> CouchbaseStream<N1qlResult>
        where S: Into<String>
    {
        let (tx, rx) = unbounded();

        let params = unsafe { lcb_n1p_new() };
        let mut cmd: lcb_CMDN1QL = unsafe { ::std::mem::zeroed() };
        cmd.callback = Some(n1ql_callback);

        let query = query.into();
        let query_length = query.len();
        let cquery = CString::new(query).unwrap();

        let tx_boxed = Box::new(tx);
        unsafe {
            lcb_n1p_setquery(params,
                             cquery.as_ptr(),
                             query_length,
                             LCB_N1P_QUERY_STATEMENT as i32);
            lcb_n1p_mkcmd(params, &mut cmd);

            let guard = self.instance.lock();
            let instance = guard.inner.unwrap();
            lcb_n1ql_query(instance,
                           Box::into_raw(tx_boxed) as *const std::os::raw::c_void,
                           &cmd);
        }

        self.unpark_io();
        CouchbaseStream::new(rx)
    }

    pub fn query_view<S>(&self, design: S, view: S, options: S) -> CouchbaseStream<ViewResult>
        where S: Into<String>
    {
        let (tx, rx) = unbounded();

        let cdesign = CString::new(design.into()).unwrap();
        let cview = CString::new(view.into()).unwrap();
        let coptions = CString::new(options.into()).unwrap();

        let mut cmd: lcb_CMDVIEWQUERY = unsafe { ::std::mem::zeroed() };
        let tx_boxed = Box::new(tx);
        unsafe {
            lcb_view_query_initcmd(&mut cmd,
                                   cdesign.as_ptr(),
                                   cview.as_ptr(),
                                   coptions.as_ptr(),
                                   Some(view_callback));

            let guard = self.instance.lock();
            let instance = guard.inner.unwrap();
            lcb_view_query(instance,
                           Box::into_raw(tx_boxed) as *const std::os::raw::c_void,
                           &cmd);
        }

        self.unpark_io();
        CouchbaseStream::new(rx)
    }
}

impl Drop for Bucket {
    fn drop(&mut self) {
        // stop the IO loop
        self.io_running.clone().store(false, Ordering::Release);
        self.unpark_io();

        // wait until the IO thread is dead
        let mut unlocked_handle = self.io_handle.lock();
        unlocked_handle.take().unwrap().join().unwrap();

        // finally, destroy the instance
        let mut guard = self.instance.lock();
        let instance = guard.inner.take().unwrap();
        unsafe { lcb_destroy(instance) };
    }
}

struct SendPtr<T> {
    inner: Option<T>,
}

unsafe impl<T> Send for SendPtr<T> {}

unsafe extern "C" fn get_callback(_instance: lcb_t, _cbtype: i32, res: *const lcb_RESPBASE) {
    let res = *(res as *const lcb_RESPGET);
    let tx = Box::from_raw(res.cookie as FutureSender<Document>);
    let result = match res.rc {
        LCB_SUCCESS => {
            let lcb_id = slice::from_raw_parts(res.key as *const u8, res.nkey).to_owned();
            let id = String::from_utf8(lcb_id).expect("Document ID is not UTF-8 compatible!");
            let content = slice::from_raw_parts(res.value as *const u8, res.nvalue).to_owned();
            Ok(Document::from_vec_with_cas(id, content, res.cas))
        }
        rc => Err(rc.into()),
    };
    tx.complete(result);
}

unsafe extern "C" fn store_callback(_instance: lcb_t, _cbtype: i32, res: *const lcb_RESPBASE) {
    let tx = Box::from_raw((*res).cookie as FutureSender<Document>);
    let result = match (*res).rc {
        LCB_SUCCESS => {
            let lcb_id = slice::from_raw_parts((*res).key as *const u8, (*res).nkey).to_owned();
            let id = String::from_utf8(lcb_id).expect("Document ID is not UTF-8 compatible!");
            Ok(Document::from_vec_with_cas(id, vec![], (*res).cas))
        }
        rc => Err(rc.into()),
    };
    tx.complete(result);
}

unsafe extern "C" fn remove_callback(_instance: lcb_t, _cbtype: i32, res: *const lcb_RESPBASE) {
    let tx = Box::from_raw((*res).cookie as FutureSender<()>);
    let result = match (*res).rc {
        LCB_SUCCESS => Ok(()),
        rc => Err(rc.into()),
    };
    tx.complete(result);
}

unsafe extern "C" fn n1ql_callback(_instance: lcb_t, _cbtype: i32, res: *const lcb_RESPN1QL) {
    let tx = Box::from_raw((*res).cookie as StreamSender<N1qlResult>);
    let lcb_row = slice::from_raw_parts((*res).row as *const u8, (*res).nrow);

    let more_to_come = ((*res).rflags as u32 & LCB_RESP_F_FINAL as u32) == 0;
    let result = if more_to_come {
        N1qlResult::Row(N1qlRow::new(String::from_utf8(lcb_row.to_owned())
            .expect("N1QL Row failed UTF-8 validation!")))
    } else {
        N1qlResult::Meta(serde_json::from_slice(lcb_row).expect("N1QL Meta decoding failed!"))
    };

    tx.send(Ok(result)).expect("Could not send N1qlResult into Stream!");
    if more_to_come {
        Box::into_raw(tx);
    }
}

unsafe extern "C" fn view_callback(_instance: lcb_t, _cbtype: i32, res: *const lcb_RESPVIEWQUERY) {
    let tx = Box::from_raw((*res).cookie as StreamSender<ViewResult>);

    let more_to_come = ((*res).rflags as u32 & LCB_RESP_F_FINAL as u32) == 0;
    let lcb_value = slice::from_raw_parts((*res).value as *const u8, (*res).nvalue);
    let result = if more_to_come {
        let lcb_key = slice::from_raw_parts((*res).key as *const u8, (*res).nkey);
        let lcb_docid = slice::from_raw_parts((*res).docid as *const u8, (*res).ndocid);
        ViewResult::Row(ViewRow {
            id: String::from_utf8(lcb_docid.to_owned())
                .expect("View DocId failed UTF-8 validation!"),
            value: String::from_utf8(lcb_value.to_owned())
                .expect("View Row failed UTF-8 validation!"),
            key: String::from_utf8(lcb_key.to_owned()).expect("View Key failed UTF-8 validation!"),
        })
    } else {
        ViewResult::Meta(ViewMeta {
            inner: String::from_utf8(lcb_value.to_owned())
                .expect("View Row failed UTF-8 validation!"),
        })
    };

    tx.send(Ok(result)).expect("Could not send ViewResult into Stream!");
    if more_to_come {
        Box::into_raw(tx);
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn remove_callback_should_handle_success() {

        // 1) create fake instance
        // 2) create fake respbase with sender cookie
        // 3) call remove_callback
        // 4) assert completion is done properly
    }

    #[test]
    fn remove_callback_should_handle_failure() {}
}
