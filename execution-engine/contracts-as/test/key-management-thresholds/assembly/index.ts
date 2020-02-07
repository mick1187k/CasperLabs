import * as CL from "../../../../contract-as/assembly";
import {Error, ErrorCode} from "../../../../contract-as/assembly/error";
import {fromBytesString} from "../../../../contract-as/assembly/bytesrepr";
import {addAssociatedKey, AddKeyFailure,
        setActionThreshold, ActionType, SetThresholdFailure,
        updateAssociatedKey, UpdateKeyFailure,
        removeAssociatedKey, RemoveKeyFailure} from "../../../../contract-as/assembly/account";

export function call(): void {
  let stageBytes = CL.getArg(0);
  if (stageBytes === null) {
    Error.fromErrorCode(ErrorCode.MissingArgument).revert();
    return;
  }

  let stage = fromBytesString(stageBytes);
  if (stage === null) {
    Error.fromErrorCode(ErrorCode.InvalidArgument).revert();
    return;
  }
  let key42s = new Array<u8>(32);
  key42s.fill(42);

  let key43s = new Array<u8>(32);
  key43s.fill(43);

  let key1s = new Array<u8>(32);
  key1s.fill(1);


  if (stage == "init") {
    if (addAssociatedKey(key42s, 100) != AddKeyFailure.Ok) {
      Error.fromUserError(4464).revert();
      return;
    }
    if (addAssociatedKey(key43s, 1) != AddKeyFailure.Ok) {
      Error.fromUserError(4464 + 1).revert();
      return;
    }
    if (addAssociatedKey(key1s, 1) != AddKeyFailure.Ok) {
      Error.fromUserError(4464 + 2).revert();
      return;
    }

    if (setActionThreshold(ActionType.KeyManagement, 101) != SetThresholdFailure.Ok) {
      Error.fromUserError(4464 + 3).revert();
      return;
    }
  }
  else if (stage == "test-permission-denied") {
    let key44s = new Array<u8>(32);
    key44s.fill(44);
    switch (addAssociatedKey(key44s, 1)) {
      case AddKeyFailure.Ok:
        Error.fromUserError(200).revert();
        break;
      case AddKeyFailure.PermissionDenied:
        break;
      default:
        Error.fromUserError(201).revert();
        break;
    }

    let key43s = new Array<u8>(32);
    key43s.fill(43);
    switch (updateAssociatedKey(key43s, 2)) {
      case UpdateKeyFailure.Ok:
        Error.fromUserError(300).revert();
        break;
      case UpdateKeyFailure.PermissionDenied:
        break;
      default:
        Error.fromUserError(301).revert();
        break;
    }

    switch (removeAssociatedKey(key43s)) {
      case RemoveKeyFailure.Ok:
        Error.fromUserError(400).revert();
        break;
      case RemoveKeyFailure.PermissionDenied:
        break;
      default:
        Error.fromUserError(401).revert();
        break;
    }

    switch (setActionThreshold(ActionType.KeyManagement, 255)) {
      case SetThresholdFailure.Ok:
        Error.fromUserError(500).revert();
        break;
      case SetThresholdFailure.PermissionDeniedError:
        break;
      default:
        Error.fromUserError(501).revert();
        break;
    }
  }
  else if (stage == "test-key-mgmnt-succeed") {
    let key44s = new Array<u8>(32);
    key44s.fill(44);

    // Has to be executed with keys of total weight >= 254
    if (addAssociatedKey(key44s, 1) != AddKeyFailure.Ok) {
      Error.fromUserError(4464 + 4).revert();
      return;
    }

    // Updates [43;32] key weight created in init stage
    if (updateAssociatedKey(key44s, 2) != UpdateKeyFailure.Ok) {
      Error.fromUserError(4464 + 5).revert();
      return;
    }
    // Removes [43;32] key created in init stage
    if (removeAssociatedKey(key44s) != RemoveKeyFailure.Ok) {
      Error.fromUserError(4464 + 6).revert();
      return;
    }
    // Sets action threshodl
    if (setActionThreshold(ActionType.KeyManagement, 100) != SetThresholdFailure.Ok) {
      Error.fromUserError(4464 + 7).revert();
      return;
    }
  }
  else {
    Error.fromUserError(1).revert();
  }
}
