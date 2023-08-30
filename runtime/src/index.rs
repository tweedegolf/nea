use crate::IoResources;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionIndex {
    pub identifier: u32,
    pub index: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketIndex {
    pub identifier: u32,
    pub index: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueIndex {
    pub identifier: u32,
    pub index: u32,
}

impl QueueIndex {
    pub(crate) fn to_bucket_index(&self, io_resources: IoResources) -> BucketIndex {
        BucketIndex {
            identifier: self.identifier,
            index: self.index / io_resources.per_bucket() as u32,
        }
    }

    pub(crate) fn to_connection_index(&self, io_resources: IoResources) -> ConnectionIndex {
        let bucket_index = self.index / io_resources.per_bucket() as u32;
        let connection =
            self.index % io_resources.per_bucket() as u32 - io_resources.tcp_streams as u32;

        ConnectionIndex {
            identifier: self.identifier,
            index: bucket_index * io_resources.http_connections as u32 + connection,
        }
    }

    pub fn from_bucket_index(io_resources: IoResources, bucket_index: BucketIndex) -> Self {
        Self {
            identifier: bucket_index.identifier,
            index: bucket_index.index * io_resources.per_bucket() as u32,
        }
    }

    pub(crate) fn from_connection_index(
        io_resources: IoResources,
        connection_index: ConnectionIndex,
    ) -> Self {
        let bucket_index = connection_index.index / io_resources.http_connections as u32;
        let connection = connection_index.index % io_resources.http_connections as u32;

        let queue_index = bucket_index * io_resources.per_bucket() as u32
            + io_resources.tcp_streams as u32
            + connection;

        Self {
            identifier: connection_index.identifier,
            index: queue_index,
        }
    }

    pub(crate) fn to_usize(self) -> usize {
        let a = self.index.to_ne_bytes();
        let b = self.identifier.to_ne_bytes();

        let bytes = [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]];

        usize::from_ne_bytes(bytes)
    }

    pub fn to_ptr(self) -> *const () {
        self.to_usize() as *const ()
    }

    pub(crate) fn from_usize(word: usize) -> Self {
        let bytes = word.to_ne_bytes();

        let [a0, a1, a2, a3, b0, b1, b2, b3] = bytes;

        let index = u32::from_ne_bytes([a0, a1, a2, a3]);
        let identifier = u32::from_ne_bytes([b0, b1, b2, b3]);

        Self { identifier, index }
    }

    pub(crate) fn from_ptr(ptr: *const ()) -> Self {
        Self::from_usize(ptr as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoIndex {
    // a bucket index
    InputStream(BucketIndex),
    CustomStream(usize),
    // an index into the global vector of http connection futures
    HttpConnection(ConnectionIndex),
}

impl IoIndex {
    pub(crate) fn from_index(resources: IoResources, queue_index: QueueIndex) -> Self {
        let index = queue_index.index as usize;
        let total_per_bucket = resources.tcp_streams + resources.http_connections;

        match index % total_per_bucket {
            0 => IoIndex::InputStream(queue_index.to_bucket_index(resources)),
            n if (1..resources.tcp_streams).contains(&n) => todo!(),
            _ => IoIndex::HttpConnection(queue_index.to_connection_index(resources)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_to_bucket_1() {
        let io_resources = IoResources {
            tcp_streams: 1,
            http_connections: 4,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 0,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::InputStream(BucketIndex {
                identifier: 0,
                index: 0
            })
        );

        let queue_index = QueueIndex {
            identifier: 1,
            index: 5,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::InputStream(BucketIndex {
                identifier: 1,
                index: 1
            })
        );
    }

    #[test]
    fn queue_to_bucket_2() {
        let io_resources = IoResources {
            tcp_streams: 2,
            http_connections: 4,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 0,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::InputStream(BucketIndex {
                identifier: 0,
                index: 0
            })
        );

        //        let queue_index = QueueIndex {
        //            identifier: 1,
        //            index: 1,
        //        };
        //
        //        assert_eq!(
        //            IoIndex::from_index(io_resources, queue_index),
        //            IoIndex::InputStream(BucketIndex {
        //                identifier: 1,
        //                index: 1
        //            })
        //        );

        let queue_index = QueueIndex {
            identifier: 1,
            index: 6,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::InputStream(BucketIndex {
                identifier: 1,
                index: 1
            })
        );
    }

    #[test]
    fn queue_to_connection() {
        let io_resources = IoResources {
            tcp_streams: 1,
            http_connections: 4,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 1,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::HttpConnection(ConnectionIndex {
                identifier: 0,
                index: 0
            })
        );

        let queue_index = QueueIndex {
            identifier: 0,
            index: 2,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::HttpConnection(ConnectionIndex {
                identifier: 0,
                index: 1
            })
        );

        let queue_index = QueueIndex {
            identifier: 0,
            index: 6,
        };

        assert_eq!(
            IoIndex::from_index(io_resources, queue_index),
            IoIndex::HttpConnection(ConnectionIndex {
                identifier: 0,
                index: 4
            })
        );
    }

    #[test]
    fn connection_to_queue() {
        let io_resources = IoResources {
            tcp_streams: 1,
            http_connections: 4,
        };

        let connection_index = ConnectionIndex {
            identifier: 0,
            index: 0,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 1,
        };

        assert_eq!(
            QueueIndex::from_connection_index(io_resources, connection_index),
            queue_index,
        );

        let connection_index = ConnectionIndex {
            identifier: 0,
            index: 1,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 2,
        };

        assert_eq!(
            QueueIndex::from_connection_index(io_resources, connection_index),
            queue_index,
        );

        let connection_index = ConnectionIndex {
            identifier: 0,
            index: 4,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 6,
        };

        assert_eq!(
            QueueIndex::from_connection_index(io_resources, connection_index),
            queue_index,
        );
    }

    #[test]
    fn bucket_to_queue() {
        let io_resources = IoResources {
            tcp_streams: 1,
            http_connections: 4,
        };

        let bucket_index = BucketIndex {
            identifier: 0,
            index: 0,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 0,
        };

        assert_eq!(
            QueueIndex::from_bucket_index(io_resources, bucket_index),
            queue_index,
        );

        let bucket_index = BucketIndex {
            identifier: 1,
            index: 1,
        };

        let queue_index = QueueIndex {
            identifier: 1,
            index: 5,
        };

        assert_eq!(
            QueueIndex::from_bucket_index(io_resources, bucket_index),
            queue_index,
        );

        let io_resources = IoResources {
            tcp_streams: 2,
            http_connections: 4,
        };

        let bucket_index = BucketIndex {
            identifier: 0,
            index: 0,
        };

        let queue_index = QueueIndex {
            identifier: 0,
            index: 0,
        };

        assert_eq!(
            QueueIndex::from_bucket_index(io_resources, bucket_index),
            queue_index,
        );

        let bucket_index = BucketIndex {
            identifier: 1,
            index: 1,
        };

        let queue_index = QueueIndex {
            identifier: 1,
            index: 6,
        };

        assert_eq!(
            QueueIndex::from_bucket_index(io_resources, bucket_index),
            queue_index,
        );
    }
}
