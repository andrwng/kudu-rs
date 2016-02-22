extern crate kudu_sys;

use std::error;
use std::fmt;
use std::ptr;
use std::result;
use std::slice;
use std::str;
use std::time::Duration;

pub use kudu_sys::{DataType, CompressionType, EncodingType};

unsafe fn str_into_kudu_slice(s: &str) -> kudu_sys::kudu_slice {
    kudu_sys::kudu_slice { data: s.as_ptr(), len: s.len() }
}

unsafe fn kudu_slice_into_slice<'a>(s: kudu_sys::kudu_slice) -> &'a [u8] {
    slice::from_raw_parts(s.data, s.len)
}

unsafe fn kudu_slice_into_str<'a>(s: kudu_sys::kudu_slice) -> &'a str {
    // TODO: Check if Kudu has a UTF-8 invariant (and fix it if not).
    str::from_utf8(kudu_slice_into_slice(s)).unwrap()
}

pub struct Error {
    inner: *const kudu_sys::kudu_status,
}

impl Error {
    pub fn code(&self) -> i8 {
        unsafe {
            kudu_sys::kudu_status_code(self.inner)
        }
    }
    pub fn posix_code(&self) -> i16 {
        unsafe {
            kudu_sys::kudu_status_posix_code(self.inner)
        }
    }
    pub fn message(&self) -> &str {
        unsafe {
            kudu_slice_into_str(kudu_sys::kudu_status_message(self.inner))
        }
    }
    fn from_status(status: *const kudu_sys::kudu_status) -> Result<()> {
        if status == ptr::null() { return Ok(()) }
        else { return Err(Error { inner: status }) }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        self.message()
    }

    fn cause(&self) -> Option<&error::Error> {
        None
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl Drop for Error {
    fn drop(&mut self) {
        unsafe {
            kudu_sys::kudu_status_destroy(self.inner);
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

pub struct ClientBuilder {
    inner: *mut kudu_sys::kudu_client_builder,
}

impl ClientBuilder {
    pub fn new() -> ClientBuilder {
        ClientBuilder {
            inner: unsafe { kudu_sys::kudu_client_builder_create() },
        }
    }

    pub fn add_master_server_addr(&mut self, addr: &str) -> &mut ClientBuilder {
        // TODO: consider taking ToSocketAddrs instead
        // TODO: handle null error properly
        unsafe {
            kudu_sys::kudu_client_builder_add_master_server_addr(self.inner,
                                                                 str_into_kudu_slice(addr));
        }
        self
    }

    pub fn clear_master_server_addrs(&mut self) -> &mut ClientBuilder {
        unsafe {
            kudu_sys::kudu_client_builder_clear_master_server_addrs(self.inner);
        }
        self
    }

    pub fn set_default_admin_operation_timeout(&mut self, timeout: &Duration) -> &mut ClientBuilder {
        unsafe {
            kudu_sys::kudu_client_builder_set_default_admin_operation_timeout(
                self.inner,
                timeout.as_secs() as i64 * 1_000 + timeout.subsec_nanos() as i64 / 1_000_000);
        }
        self
    }

    pub fn set_default_rpc_timeout(&mut self, timeout: &Duration) -> &mut ClientBuilder {
        unsafe {
            kudu_sys::kudu_client_builder_set_default_rpc_timeout(
                self.inner,
                timeout.as_secs() as i64 * 1_000 + timeout.subsec_nanos() as i64 / 1_000_000);
        }
        self
    }

    // TODO: does this need to consume the builder?
    pub fn build(self) -> Result<Client> {
        let client = ptr::null_mut();;
        unsafe {
            try!(Error::from_status(kudu_sys::kudu_client_builder_build(self.inner, &client)));
        }

        Ok(Client {
            inner: client,
        })
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ClientBuilder")
    }
}

impl Drop for ClientBuilder {
    fn drop(&mut self) {
        unsafe {
            kudu_sys::kudu_client_builder_destroy(self.inner);
        }
    }
}

pub struct Client {
    inner: *mut kudu_sys::kudu_client,
}

impl Client {

    pub fn list_tables(&self) -> Result<Vec<&str>> {
        unsafe {
            let list = ptr::null_mut();
            try!(Error::from_status(kudu_sys::kudu_client_list_tables(self.inner, &list)));
            let size = kudu_sys::kudu_table_list_size(list);
            let mut tables = Vec::with_capacity(size);

            for i in 0..size {
                tables.push(kudu_slice_into_str(kudu_sys::kudu_table_list_table_name(list, i)));
            }
            kudu_sys::kudu_table_list_destroy(list);
            Ok(tables)
        }
    }

    pub fn table_schema(&self, table: &str) -> Result<Schema> {
        unsafe {
            let schema = ptr::null_mut();
            try!(Error::from_status(kudu_sys::kudu_client_table_schema(self.inner,
                                                                       str_into_kudu_slice(table),
                                                                       &schema)));

            Ok(Schema { inner: schema })
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Client")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            kudu_sys::kudu_client_destroy(self.inner);
        }
    }
}

pub struct Schema {
    inner: *mut kudu_sys::kudu_schema,
}

impl Schema {
    pub fn num_columns(&self) -> usize {
        unsafe {
            return kudu_sys::kudu_schema_num_columns(self.inner);
        }
    }

    pub fn num_primary_key_columns(&self) -> usize {
        unsafe {
            return kudu_sys::kudu_schema_num_key_columns(self.inner);
        }
    }

    pub fn column(&self, index: usize) -> ColumnSchema {
        ColumnSchema {
            inner: unsafe { kudu_sys::kudu_schema_column(self.inner, index) },
        }
    }
}

impl fmt::Debug for Schema {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Schema")
    }
}

impl Drop for Schema {
    fn drop(&mut self) {
        unsafe {
            kudu_sys::kudu_schema_destroy(self.inner);
        }
    }
}

pub struct ColumnSchema {
    inner: *mut kudu_sys::kudu_column_schema,
}

impl ColumnSchema {
    pub fn name(&self) -> &str {
        unsafe {
            kudu_slice_into_str(kudu_sys::kudu_column_schema_name(self.inner))
        }
    }

    pub fn data_type(&self) -> DataType {
        unsafe {
            kudu_sys::kudu_column_schema_type(self.inner)
        }
    }

    pub fn is_nullable(&self) -> bool {
        unsafe {
            kudu_sys::kudu_column_schema_is_nullable(self.inner) != 0
        }
    }
}

impl fmt::Debug for ColumnSchema {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ColumnSchema({})", self.name())
    }
}

impl Drop for ColumnSchema {
    fn drop(&mut self) {
        unsafe {
            kudu_sys::kudu_column_schema_destroy(self.inner);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_unreachable_master() {
        let mut builder = ClientBuilder::new();
        builder.add_master_server_addr("kudu.example.com");
        let client = builder.build();
        println!("client: {:?}", client);
        assert!(client.is_err());
    }
}
