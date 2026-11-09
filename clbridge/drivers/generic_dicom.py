# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Generic DICOM Print Driver (PS3.4 §H) — implements AbstractPrintDriver.

Works with any standard DICOM printer supporting:
  - Basic Film Session SOP Class (1.2.840.10008.5.1.1.1)
  - Basic Film Box SOP Class (1.2.840.10008.5.1.1.2)
  - Basic Grayscale Image Box SOP Class (1.2.840.10008.5.1.1.4)
  - Basic Color Image Box SOP Class (1.2.840.10008.5.1.1.4.1)
  - Printer SOP Class (1.2.840.10008.5.1.1.16)
"""

import io
import logging
import uuid

from pydicom import Dataset, dcmread
from pydicom.uid import generate_uid
from pynetdicom import AE
from pynetdicom.sop_class import (
    BasicColorImageBox,
    BasicFilmBox,
    BasicFilmSession,
    BasicGrayscaleImageBox,
    Printer,
)

from .base import AbstractPrintDriver, PrintResult

# DIMSE Print status codes (PS3.4 §H.4.1)
STATUS_SUCCESS = 0x0000
STATUS_FILM_SESSION_ERROR = 0xB600
STATUS_FILM_BOX_ERROR = 0xB601
STATUS_IMAGE_BOX_ERROR = 0xB602
STATUS_PRINT_FAILURE = 0xB603
STATUS_GENERIC_ERROR = 0xC200


class GenericDicomPrintDriver(AbstractPrintDriver):
    """Generic DICOM Print SCU driver (PS3.4 §H).

    Implements the standard N-CREATE Film Session → N-CREATE Film Box →
    N-SET Image Box(es) → N-ACTION Print → N-DELETE Film Session sequence.
    """

    def driver_name(self) -> str:
        return "generic_dicom"

    def print(
        self,
        film_session: Dataset,
        images: list[bytes],
        printer_host: str,
        printer_port: int,
        calling_aet: str,
    ) -> PrintResult:
        """Execute a DICOM Print job.

        See AbstractPrintDriver.print() for full documentation.
        """
        job_id = str(uuid.uuid4())[:8]
        film_boxes_printed = 0
        failed_images = []
        printer_message = None

        # Determine Image Box SOP Class from first image
        image_box_sop_class = self._image_box_sop_class(images)

        ae = AE(ae_title=calling_aet)
        ae.add_requested_context(BasicFilmSession)
        ae.add_requested_context(BasicFilmBox)
        ae.add_requested_context(image_box_sop_class)
        ae.add_requested_context(Printer)

        # 1. Association
        try:
            assoc = ae.associate(printer_host, printer_port, ae_title=calling_aet)
            if not assoc.is_established:
                logging.warning(
                    "[C-PRINT] Association to %s:%d failed",
                    printer_host,
                    printer_port,
                )
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_GENERIC_ERROR,
                    film_boxes_printed=0,
                    printer_message=f"Association to {printer_host}:{printer_port} failed",
                )
        except Exception as e:
            logging.error("[C-PRINT] Association error: %s", e)
            return PrintResult(
                job_id=job_id,
                status=STATUS_GENERIC_ERROR,
                film_boxes_printed=0,
                printer_message=f"Association error: {e}",
            )

        film_session_uid = None
        film_box_uid = None
        image_box_uids = []

        try:
            # 2. N-CREATE Film Session
            film_session_uid = generate_uid()
            fs_ds = Dataset()
            # Copy all attributes from the supplied film_session
            for elem in film_session:
                if elem.keyword and elem.keyword not in (
                    "SOPClassUID",
                    "SOPInstanceUID",
                ):
                    setattr(fs_ds, elem.keyword, elem.value)
            fs_ds.SOPClassUID = BasicFilmSession
            fs_ds.SOPInstanceUID = film_session_uid

            status, fs_response = assoc.send_n_create(fs_ds, BasicFilmSession)
            status_code = self._status_code(status)
            if status_code != STATUS_SUCCESS:
                logging.warning(
                    "[C-PRINT] N-CREATE Film Session failed: status=0x%04X", status_code
                )
                # N-DELETE cleanup not needed — nothing to clean
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_FILM_SESSION_ERROR,
                    film_boxes_printed=0,
                    printer_message=f"N-CREATE Film Session failed: 0x{status_code:04X}",
                )

            logging.debug("[C-PRINT] Film Session created: %s", film_session_uid)

            # 3. N-CREATE Film Box
            film_box_uid = generate_uid()
            fb_ds = Dataset()
            fb_ds.SOPClassUID = BasicFilmBox
            fb_ds.SOPInstanceUID = film_box_uid

            # ReferencedFilmSessionSequence
            ref_fs_item = Dataset()
            ref_fs_item.ReferencedSOPClassUID = BasicFilmSession
            ref_fs_item.ReferencedSOPInstanceUID = film_session_uid
            fb_ds.ReferencedFilmSessionSequence = [ref_fs_item]

            # ImageDisplayFormat — default to "STANDARD\\1,1" if not in film_session
            display_format = getattr(
                film_session, "ImageDisplayFormat", "STANDARD\\1,1"
            )
            fb_ds.ImageDisplayFormat = display_format

            # FilmBoxContentsSequence if present
            if hasattr(film_session, "FilmBoxContentsSequence"):
                fb_ds.FilmBoxContentsSequence = film_session.FilmBoxContentsSequence

            # Copy relevant film session attributes to film box
            for attr in (
                "FilmOrientation",
                "FilmSizeID",
                "MagnificationType",
                "MaxDensity",
                "MinDensity",
                "ConfigurationInformation",
                "AnnotationDisplayFormatID",
                "SmoothingType",
                "BorderDensity",
                "EmptyImageDensity",
                "Trim",
            ):
                if hasattr(film_session, attr):
                    setattr(fb_ds, attr, getattr(film_session, attr))

            status, fb_response = assoc.send_n_create(fb_ds, BasicFilmBox)
            status_code = self._status_code(status)
            if status_code != STATUS_SUCCESS:
                logging.warning(
                    "[C-PRINT] N-CREATE Film Box failed: status=0x%04X", status_code
                )
                # Cleanup: N-DELETE Film Session
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_FILM_BOX_ERROR,
                    film_boxes_printed=0,
                    printer_message=f"N-CREATE Film Box failed: 0x{status_code:04X}",
                )

            logging.debug("[C-PRINT] Film Box created: %s", film_box_uid)

            # 4. N-SET Image Box × N
            for idx, image_bytes in enumerate(images):
                image_box_uid = generate_uid()
                image_box_uids.append(image_box_uid)

                ib_ds = Dataset()
                ib_ds.SOPClassUID = image_box_sop_class
                ib_ds.SOPInstanceUID = image_box_uid

                # ReferencedFilmBoxSequence
                ref_fb_item = Dataset()
                ref_fb_item.ReferencedSOPClassUID = BasicFilmBox
                ref_fb_item.ReferencedSOPInstanceUID = film_box_uid
                ib_ds.ReferencedFilmBoxSequence = [ref_fb_item]

                # Image pixel data in appropriate sequence
                try:
                    img_ds = dcmread(io.BytesIO(image_bytes), force=True)
                except Exception as e:
                    logging.warning("[C-PRINT] Failed to parse image %d: %s", idx, e)
                    failed_images.append((idx, f"Parse error: {e}"))
                    continue

                if image_box_sop_class == BasicColorImageBox:
                    ib_ds.BasicColorImageSequence = [img_ds]
                else:
                    ib_ds.BasicGrayscaleImageSequence = [img_ds]

                status, ib_response = assoc.send_n_set(
                    ib_ds, image_box_sop_class, image_box_uid
                )
                status_code = self._status_code(status)
                if status_code != STATUS_SUCCESS:
                    logging.warning(
                        "[C-PRINT] N-SET Image Box %d failed: status=0x%04X",
                        idx,
                        status_code,
                    )
                    failed_images.append((idx, f"N-SET error: 0x{status_code:04X}"))
                else:
                    logging.debug("[C-PRINT] Image Box %d set: %s", idx, image_box_uid)

            # 5. N-ACTION Print
            if image_box_uids:
                na_ds = Dataset()
                # ReferencedFilmBoxSequence
                ref_fb_item = Dataset()
                ref_fb_item.ReferencedSOPClassUID = BasicFilmBox
                ref_fb_item.ReferencedSOPInstanceUID = film_box_uid
                na_ds.ReferencedFilmBoxSequence = [ref_fb_item]

                # Also include PrintManagementStatusSequence if in film_session
                if hasattr(film_session, "PrintManagementStatusSequence"):
                    na_ds.PrintManagementStatusSequence = (
                        film_session.PrintManagementStatusSequence
                    )

                status, na_response = assoc.send_n_action(
                    na_ds,
                    action_type=1,
                    class_uid=BasicFilmSession,
                    instance_uid=film_session_uid,
                )
                status_code = self._status_code(status)
                if status_code != STATUS_SUCCESS:
                    logging.warning(
                        "[C-PRINT] N-ACTION Print failed: status=0x%04X", status_code
                    )
                    # Extract printer message from response if available
                    if na_response:
                        printer_message = self._extract_status_info(na_response)
                    assoc.release()
                    return PrintResult(
                        job_id=job_id,
                        status=STATUS_PRINT_FAILURE,
                        film_boxes_printed=0,
                        failed_images=failed_images,
                        printer_message=printer_message
                        or f"N-ACTION Print failed: 0x{status_code:04X}",
                    )

                # Extract printer status info from N-ACTION response
                if na_response:
                    printer_message = self._extract_status_info(na_response)

                logging.info(
                    "[C-PRINT] Print job %s: N-ACTION succeeded, %d images",
                    job_id,
                    len(image_box_uids),
                )

            # Count successfully printed film boxes
            film_boxes_printed = 1 if image_box_uids else 0

            # Determine final status
            if failed_images and len(failed_images) == len(images):
                final_status = STATUS_IMAGE_BOX_ERROR
            elif failed_images:
                final_status = STATUS_SUCCESS  # partial success = success with warnings
            else:
                final_status = STATUS_SUCCESS

            return PrintResult(
                job_id=job_id,
                status=final_status,
                film_boxes_printed=film_boxes_printed,
                failed_images=failed_images,
                printer_message=printer_message,
            )

        except Exception as e:
            logging.error("[C-PRINT] Print job %s failed: %s", job_id, e, exc_info=True)
            return PrintResult(
                job_id=job_id,
                status=STATUS_GENERIC_ERROR,
                film_boxes_printed=0,
                failed_images=failed_images,
                printer_message=str(e),
            )

        finally:
            # 6. N-DELETE Film Session (cleanup — always!)
            if film_session_uid:
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)

            # 7. Release association
            if assoc.is_established:
                assoc.release()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _image_box_sop_class(self, images: list[bytes]) -> str:
        """Determine Image Box SOP Class from first image's channels.

        Args:
            images: List of raw DICOM Part 10 byte strings.

        Returns:
            BasicGrayscaleImageBox (1 channel) or BasicColorImageBox (3 channels / RGB).
        """
        if not images:
            return BasicGrayscaleImageBox  # default
        try:
            ds = dcmread(io.BytesIO(images[0]), force=True)
            spp = getattr(ds, "SamplesPerPixel", 1)
            photometric = getattr(ds, "PhotometricInterpretation", "MONOCHROME2")
            if spp == 3 or photometric in ("RGB", "YBR_FULL", "YBR_FULL_422"):
                return BasicColorImageBox
        except Exception as e:
            logging.debug(
                "[C-PRINT] Failed to parse image for SOP class detection: %s", e
            )
        return BasicGrayscaleImageBox

    @staticmethod
    def _status_code(status) -> int:
        """Extract integer status code from pynetdicom Status object.

        Handles both Status objects (with .Status attribute) and raw integers.
        """
        if status is None:
            return STATUS_SUCCESS
        if hasattr(status, "Status"):
            return status.Status
        return int(status)

    @staticmethod
    def _safe_n_delete(assoc, sop_class_uid, sop_instance_uid):
        """Send N-DELETE, logging but not raising on failure."""
        try:
            status, _ = assoc.send_n_delete(sop_class_uid, sop_instance_uid)
            sc = status.Status if hasattr(status, "Status") else status
            if sc != STATUS_SUCCESS:
                logging.debug(
                    "[C-PRINT] N-DELETE returned status=0x%04X for %s",
                    sc,
                    sop_instance_uid,
                )
            else:
                logging.debug("[C-PRINT] Film Session deleted: %s", sop_instance_uid)
        except Exception as e:
            logging.debug("[C-PRINT] N-DELETE failed (non-critical): %s", e)

    @staticmethod
    def _extract_status_info(dataset) -> str | None:
        """Extract human-readable status info from N-ACTION-RSP dataset.

        Looks at PrintManagementStatusSequence or StatusInformation.
        """
        if dataset is None:
            return None
        try:
            # Try StatusInformation (0000,0902)
            si = getattr(dataset, "StatusInformation", None)
            if si:
                return str(si)
            # Try PrintManagementStatusSequence
            pm_seq = getattr(dataset, "PrintManagementStatusSequence", None)
            if pm_seq:
                parts = []
                for item in pm_seq:
                    info = getattr(item, "PrinterStatusInfo", None)
                    if info:
                        parts.append(str(info))
                if parts:
                    return "; ".join(parts)
        except Exception:
            pass
        return None
