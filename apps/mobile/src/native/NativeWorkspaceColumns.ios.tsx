import { requireOptionalNativeModule } from "expo";
import { Platform } from "react-native";
const nativeControls = requireOptionalNativeModule<{
  readonly supportsWorkspaceColumns?: boolean;
  readonly duoEnabled?: boolean;
}>("T3NativeControls");
export const NATIVE_WORKSPACE_COLUMNS_SUPPORTED =
  Platform.OS === "ios" &&
  (Platform.isPad ||
    (nativeControls?.duoEnabled === true && Number.parseFloat(String(Platform.Version)) >= 27.1)) &&
  nativeControls?.supportsWorkspaceColumns === true;
