//! Scoped, immutable file views for executable-image discovery.
use super::*;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY,
    UnmapViewOfFile,
};

pub(super) struct ImageView {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    _mapping: OwnedHandle,
    _file: fs::File,
    len: usize,
}

impl ImageView {
    pub(super) fn open(path: &Path) -> Result<Self> {
        // Deny writes/deletion while borrowed slices exist. Discovery can
        // scan the image without allocating a second, file-sized heap copy.
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)?;
        let len = usize::try_from(file.metadata()?.len())?;
        anyhow::ensure!(
            len > 0 && len <= isize::MAX as usize,
            "Invalid Studio image size"
        );
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle() as HANDLE,
                null(),
                PAGE_READONLY,
                0,
                0,
                null(),
            )
        };
        anyhow::ensure!(
            !mapping.is_null(),
            "Cannot map Studio image: {}",
            io::Error::last_os_error()
        );
        let mapping = unsafe { OwnedHandle::from_raw_handle(mapping) };
        let view = unsafe { MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_READ, 0, 0, len) };
        anyhow::ensure!(
            !view.Value.is_null(),
            "Cannot read Studio image mapping: {}",
            io::Error::last_os_error()
        );
        Ok(Self {
            view,
            _mapping: mapping,
            _file: file,
            len,
        })
    }

    pub(super) fn bytes(&self) -> &[u8] {
        // The read-only mapping and write-denying file handle outlive this slice.
        unsafe { std::slice::from_raw_parts(self.view.Value.cast(), self.len) }
    }
}

impl Drop for ImageView {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_views_hold_immutable_bytes_and_release_each_file() -> Result<()> {
        let directory = crate::tests::support::temp_dir("native-image-view");
        let first = directory.join("first.exe");
        let second = directory.join("second.exe");
        let bytes = (0..65536)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&first, &bytes)?;
        fs::write(&second, b"another build")?;
        let mapped = ImageView::open(&first)?;
        let other = ImageView::open(&second)?;
        assert_eq!(mapped.bytes(), bytes);
        assert_eq!(other.bytes(), b"another build");
        assert!(fs::OpenOptions::new().write(true).open(&first).is_err());
        assert!(fs::remove_file(&first).is_err());
        assert!(fs::rename(&first, directory.join("moved.exe")).is_err());
        drop(mapped);
        fs::write(&first, b"replacement")?;
        for _ in 0..16 {
            let reopened = ImageView::open(&first)?;
            assert_eq!(reopened.bytes(), b"replacement");
        }
        fs::remove_file(&first)?;
        assert_eq!(other.bytes(), b"another build");
        drop(other);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn failed_image_discovery_releases_file_handles() -> Result<()> {
        let directory = crate::tests::support::temp_dir("native-image-failure");
        let path = directory.join("image.exe");
        assert!(ImageView::open(&path).is_err());
        fs::write(&path, [])?;
        assert!(ImageView::open(&path).is_err());
        fs::remove_file(&path)?;
        fs::write(&path, b"not a PE image")?;
        let discover = || -> Result<()> {
            let mapped = ImageView::open(&path)?;
            PeImage::parse(mapped.bytes())?;
            Ok(())
        };
        assert!(discover().is_err());
        fs::remove_dir_all(directory)?;
        Ok(())
    }
}
