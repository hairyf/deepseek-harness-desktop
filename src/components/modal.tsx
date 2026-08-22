import type { PropsWithOverlays } from '@overlastic/react'
import type { ReactNode } from 'react'
import { AlertDialog, Button } from '@heroui/react'
import { useDisclosure } from '@overlastic/react'
import { useTranslation } from 'react-i18next'

/** AlertDialog 状态色：库内 AlertDialogStatus 未从子包 re-export，本地保持同步定义 */
export type AlertDialogStatus = 'default' | 'accent' | 'success' | 'warning' | 'danger'

export interface ModalProps extends PropsWithOverlays {
  status: AlertDialogStatus
  title?: string
  description?: ReactNode

  cancelText?: string
  confirmText?: string
}

export function Modal(props: ModalProps) {
  const disclosure = useDisclosure({ props })
  const { t } = useTranslation()

  const buttonVariant = props.status === 'danger' ? 'danger' : 'primary'

  return (
    <AlertDialog onOpenChange={disclosure.cancel} isOpen={disclosure.visible}>
      <AlertDialog.Backdrop>
        <AlertDialog.Container>
          <AlertDialog.Dialog className="sm:max-w-[400px]">
            <AlertDialog.CloseTrigger />
            <AlertDialog.Header>
              <AlertDialog.Icon status={props.status} />
              <AlertDialog.Heading>{props.title}</AlertDialog.Heading>
            </AlertDialog.Header>
            <AlertDialog.Body>
              {props.description}
            </AlertDialog.Body>
            <AlertDialog.Footer>
              <Button className="rounded-md" variant="tertiary" onPress={disclosure.cancel}>
                {props.cancelText || t('buttons.cancel')}
              </Button>
              <Button className="rounded-md" variant={buttonVariant} onPress={disclosure.confirm}>
                {props.confirmText || t('buttons.confirm')}
              </Button>
            </AlertDialog.Footer>
          </AlertDialog.Dialog>
        </AlertDialog.Container>
      </AlertDialog.Backdrop>
    </AlertDialog>
  )
}
